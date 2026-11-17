//! Local adoption attitudes, authenticated reason pointers and stored observations.

use anyhow::{Context, Result, bail, ensure};
use crate::Node;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::object::Digest;
use super::codec::{MAX_ARTIFACT_BYTES, MAX_EDGES, MAX_TEXT_BYTES, UnitDescriptor};
use super::installation::{StoredVersion, now, set_config_section};
use super::trials::AttemptMeasurement;

/// How one unit version was judged on this installation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdoptionDecision {
    /// Kept and usable.
    Adopted,
    /// Not kept; the reason and the evidence stay attached to the version.
    Rejected,
    /// Kept as history but no longer offered.
    Retired,
}

impl AdoptionDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            AdoptionDecision::Adopted => "adopted",
            AdoptionDecision::Rejected => "rejected",
            AdoptionDecision::Retired => "retired",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "adopted" => AdoptionDecision::Adopted,
            "rejected" => AdoptionDecision::Rejected,
            "retired" => AdoptionDecision::Retired,
            other => bail!("unknown adoption decision {other}"),
        })
    }
}

/// How the judgement about one unit version was reached.
///
/// The method says which kind of work produced the judgement. It never
/// replaces the evidence: a method the node cannot back with a durable
/// attempt of its own stays a claim.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdoptionMethod {
    /// A finite comparison against a control excluding every version of the
    /// candidate unit id, in a real sandbox with the caller's scenario and rubric.
    SandboxComparison,
    /// A local execution trial of the installed artifact in this node's own
    /// environment.
    LocalTrial,
    /// A statement by the owner or an operator. It records what was said; it
    /// is not an empirical measurement and never claims one.
    OperatorStatement,
}

impl AdoptionMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            AdoptionMethod::SandboxComparison => "sandbox_comparison",
            AdoptionMethod::LocalTrial => "local_trial",
            AdoptionMethod::OperatorStatement => "operator_statement",
        }
    }

    /// Whether this method can ever carry empirical acceptance.
    ///
    /// Only a method that runs something can. A statement records intent;
    /// it is not a measurement.
    pub fn is_empirical(&self) -> bool {
        match self {
            AdoptionMethod::SandboxComparison | AdoptionMethod::LocalTrial => true,
            AdoptionMethod::OperatorStatement => false,
        }
    }

    /// Parse the serialized method name; an unknown method is rejected so a
    /// misspelt method never becomes an unnamed judgement.
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "sandbox_comparison" => AdoptionMethod::SandboxComparison,
            "local_trial" => AdoptionMethod::LocalTrial,
            "operator_statement" => AdoptionMethod::OperatorStatement,
            other => bail!("unknown adoption method {other}"),
        })
    }
}

/// One piece of evidence for a recorded judgement.
///
/// The node verifies each item against its own durable state. A trial
/// attempt is a runner-owned record: the node created it before the run and
/// closed it with the run's receipts. A stored observation is bytes this
/// node holds and can re-hash — proof that the bytes exist, never proof
/// that a measurement produced them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdoptionEvidence {
    /// A trial attempt of this node, bound to this unit version.
    TrialAttempt { attempt_id: String },
    /// Bytes this node stores for this unit version.
    StoredObservation { digest: Digest },
}

/// Why the installation authority refused one evidence item of a judgement.
///
/// A refusal travels as a typed payload inside the error, so a caller can
/// report the reason as a stable code instead of parsing this node's prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdoptionEvidenceRejection {
    /// The attempt id names no trial attempt this node ever opened.
    AttemptNotOpened,
    /// The attempt is bound to a different unit, revision or artifact.
    AttemptOfOtherVersion,
    /// The attempt was opened and never reached a terminal trial fact, so
    /// nothing about a measurement is recorded under it.
    AttemptNeverExecuted,
    /// The observation is not held for this exact unit version.
    ObservationNotHeld,
}

impl AdoptionEvidenceRejection {
    /// The stable result code a judgement child reports for this refusal.
    pub fn code(&self) -> &'static str {
        match self {
            AdoptionEvidenceRejection::AttemptNeverExecuted => "evidence_unexecuted",
            AdoptionEvidenceRejection::AttemptNotOpened
            | AdoptionEvidenceRejection::AttemptOfOtherVersion
            | AdoptionEvidenceRejection::ObservationNotHeld => "evidence_not_admissible",
        }
    }
}

/// One refused evidence item, with this node's own explanation.
#[derive(Debug, Clone)]
pub struct AdoptionEvidenceRejected {
    pub reason: AdoptionEvidenceRejection,
    pub detail: String,
}

impl AdoptionEvidenceRejected {
    fn refusal(reason: AdoptionEvidenceRejection, detail: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(AdoptionEvidenceRejected {
            reason,
            detail: detail.into(),
        })
    }
}

impl std::fmt::Display for AdoptionEvidenceRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for AdoptionEvidenceRejected {}

/// Tri-state result of verifying an adoption attitude's reason pointers.
///
/// It is a local verification result, never a property of the record: an
/// imported, unrelated or unexecuted reason pointer is `Undetermined`, and
/// `Undetermined` is reported as unresolved instead of being collapsed into a
/// claim of either verification or refutation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContentAdoptionEvidence {
    /// Every reason pointer was re-verified against this installation.
    VerifiedLocal,
    /// At least one reason pointer no longer holds here.
    RefutedLocal,
    /// Nothing local establishes the pointers either way.
    Undetermined,
}

impl ContentAdoptionEvidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContentAdoptionEvidence::VerifiedLocal => "verified_local",
            ContentAdoptionEvidence::RefutedLocal => "refuted_local",
            ContentAdoptionEvidence::Undetermined => "undetermined",
        }
    }
}

/// The adoption attitude a content member carries, as read from the
/// installation that holds it.
///
/// This module never stores an adoption attitude: the canonical content and
/// attitude ledger owns them. What arrives here is a read projection, and the
/// membership decision needs exactly three things — the attitude points at this
/// exact content digest, it reads as adopted, and its reason pointers verify
/// locally.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContentAdoption {
    pub attitude_id: String,
    pub content_digest: Digest,
    pub adopted: bool,
    pub evidence: ContentAdoptionEvidence,
    /// Digest of a content reason pointer, when the attitude carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_digest: Option<Digest>,
    pub recorded_at: String,
}

impl ContentAdoption {
    /// Whether this attitude lets the exact content revision join a macro.
    pub fn accepts(&self, digest: &Digest) -> bool {
        self.adopted
            && self.evidence == ContentAdoptionEvidence::VerifiedLocal
            && &self.content_digest == digest
    }

    /// Why this attitude does not admit the content, when it does not.
    pub fn refusal(&self, digest: &Digest) -> Option<String> {
        if self.accepts(digest) {
            return None;
        }
        if &self.content_digest != digest {
            return Some(format!(
                "adoption attitude {} points at {} instead of this content revision",
                self.attitude_id, self.content_digest
            ));
        }
        if !self.adopted {
            return Some(format!(
                "adoption attitude {} does not read as adopted",
                self.attitude_id
            ));
        }
        Some(match self.evidence {
            ContentAdoptionEvidence::VerifiedLocal => format!(
                "adoption attitude {} verified locally but is not adopted",
                self.attitude_id
            ),
            ContentAdoptionEvidence::RefutedLocal => format!(
                "adoption attitude {} has a reason pointer that no longer holds on this node",
                self.attitude_id
            ),
            ContentAdoptionEvidence::Undetermined => format!(
                "adoption attitude {} has no locally verified reason pointer; the evidence is undetermined, not refuted",
                self.attitude_id
            ),
        })
    }
}

/// The port through which a metapackage asks the content/attitude ledger
/// whether one exact content revision carries an admissible adoption attitude.
///
/// The canonical ledger (content, attitudes and reason pointers) implements
/// this; this module offers [`UnitAdoptionProjection`] as the projection over
/// the installation adoption records it already keeps, so nothing is duplicated
/// and no second adoption store exists.
pub trait MacroContentAdoption {
    fn adoption(
        &self,
        member_id: &str,
        revision: &str,
        digest: &Digest,
    ) -> Result<Option<ContentAdoption>>;
}

/// Read projection over this installation's own adoption records.
///
/// The record lives inside the exact retained unit version it judged: an
/// imported descriptor has its cognition section stripped on arrival, so a
/// judgement made on another node can never be read here as a local adoption.
/// Every reason pointer is re-verified now against this node's own trial and
/// observation store; a stored `verified` flag is not trusted on its own.
pub struct UnitAdoptionProjection<'a> {
    conn: &'a Connection,
}

impl<'a> UnitAdoptionProjection<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }
}

impl MacroContentAdoption for UnitAdoptionProjection<'_> {
    fn adoption(
        &self,
        member_id: &str,
        revision: &str,
        digest: &Digest,
    ) -> Result<Option<ContentAdoption>> {
        let data: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![member_id, revision, digest.0],
                |row| row.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            return Ok(None);
        };
        let stored: StoredVersion = serde_json::from_str(&data)?;
        let Some(record) = stored.unit.config.pointer("/cognition/adoption") else {
            return Ok(None);
        };
        let decision = record
            .get("decision")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let recorded_at = record
            .get("recorded_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut pointers = 0usize;
        let mut verified = 0usize;
        let mut refuted = false;
        let mut reason_digest: Option<Digest> = None;
        for item in record
            .get("evidence")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            pointers += 1;
            match item.get("kind").and_then(Value::as_str) {
                Some("trial_attempt") => {
                    let attempt_id = item
                        .get("attempt_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let holds = Node::trial_attempt_facts(self.conn, attempt_id)?.is_some_and(|facts| {
                        facts.unit_id == member_id
                            && facts.revision == revision
                            && facts.artifact_digest == *digest
                            && facts.state == "finished"
                            && facts.plan_digest.is_some()
                    });
                    if holds {
                        verified += 1;
                    } else {
                        refuted = true;
                    }
                }
                Some("stored_observation") => {
                    let Some(observed) = item.get("digest").and_then(Value::as_str) else {
                        refuted = true;
                        continue;
                    };
                    let observed = Digest(observed.to_string());
                    if Node::observation_bound(self.conn, &stored.unit, &observed)? {
                        verified += 1;
                        if reason_digest.is_none() {
                            reason_digest = Some(observed);
                        }
                    } else {
                        refuted = true;
                    }
                }
                _ => refuted = true,
            }
        }
        let evidence = if refuted {
            ContentAdoptionEvidence::RefutedLocal
        } else if pointers > 0 && verified == pointers {
            ContentAdoptionEvidence::VerifiedLocal
        } else {
            ContentAdoptionEvidence::Undetermined
        };
        Ok(Some(ContentAdoption {
            attitude_id: format!("unit-adoption:{member_id}@{revision}:{recorded_at}"),
            content_digest: digest.clone(),
            adopted: decision == AdoptionDecision::Adopted.as_str(),
            evidence,
            reason_digest,
            recorded_at,
        }))
    }
}

impl Node {
    /// Record how one unit version was judged, and on which evidence.
    ///
    /// The decision is stored under the unit's own configuration as a
    /// recorded fact. The node never derives a decision from the unit's
    /// text and never requires one: an unjudged unit stays unjudged.
    ///
    /// The record is an attitude about one immutable content revision: it
    /// names the artifact digest, the reason pointers the caller cited and
    /// how far that reason reaches. Its confidence is what the subject
    /// stated; absent means unknown, and this node never supplies a default
    /// number.
    pub(super) fn unit_record_adoption(
        &mut self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        decision: AdoptionDecision,
        method: AdoptionMethod,
        evidence: &[AdoptionEvidence],
        confidence: Option<f64>,
        notes: Option<&str>,
        enable: Option<bool>,
    ) -> Result<Value> {
        ensure!(
            evidence.len() <= MAX_EDGES,
            "adoption evidence count out of bounds"
        );
        if let Some(confidence) = confidence {
            // This node records the number the subject stated. It supplies no
            // scale and no range of its own: a bound here would be a reading
            // of the subject's confidence that the subject never made.
            ensure!(
                confidence.is_finite(),
                "the stated confidence must be a finite number; an unjudged subject states nothing and is recorded as unknown"
            );
        }
        if let Some(notes) = notes {
            ensure!(notes.len() <= MAX_TEXT_BYTES, "adoption notes exceed the text bound");
        }
        if enable == Some(true) {
            ensure!(
                decision == AdoptionDecision::Adopted,
                "only an adopted unit can be enabled"
            );
        }
        let recorded_at = now();
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        let data: Option<String> = tx
            .query_row(
                "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit_id, revision, artifact_digest.0],
                |r| r.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            bail!("unit version is not retained on this node");
        };
        let mut stored: StoredVersion = serde_json::from_str(&data)?;
        // The node verifies every evidence item against its own durable
        // state. A trial attempt is runner-owned: this node opened it before
        // the run and closed it with the run's receipts. A stored observation
        // is bytes this node holds; it proves the bytes exist, never that a
        // measurement produced them, so it can never carry empirical
        // acceptance.
        let mut resolved: Vec<Value> = Vec::new();
        // What the cited trials can carry, strongest first. A citation that
        // resolved to a recorded run is not the same fact as one that
        // resolved to a measurement, and the record says which it was.
        let mut measurement: Option<AttemptMeasurement> = None;
        for item in evidence {
            match item {
                AdoptionEvidence::TrialAttempt { attempt_id } => {
                    let attempt_id = attempt_id.trim();
                    ensure!(
                        !attempt_id.is_empty() && attempt_id.len() <= MAX_TEXT_BYTES,
                        "adoption trial attempt id is empty or too long"
                    );
                    let facts = Self::trial_attempt_facts(&tx, attempt_id)?;
                    let Some(facts) = facts else {
                        return Err(AdoptionEvidenceRejected::refusal(
                            AdoptionEvidenceRejection::AttemptNotOpened,
                            "adoption evidence names a trial attempt this node never opened",
                        ));
                    };
                    if facts.unit_id != unit_id
                        || facts.revision != revision
                        || facts.artifact_digest != *artifact_digest
                    {
                        return Err(AdoptionEvidenceRejected::refusal(
                            AdoptionEvidenceRejection::AttemptOfOtherVersion,
                            "adoption evidence names a trial attempt of a different unit version",
                        ));
                    }
                    // An attempt that was opened and never closed records no
                    // terminal fact, so citing it claims a measurement that
                    // does not exist. A closed attempt with no authenticated
                    // measurement is citable — the subject may judge on what
                    // it did not learn — but it never carries acceptance.
                    if facts.measurement == AttemptMeasurement::Unexecuted {
                        return Err(AdoptionEvidenceRejected::refusal(
                            AdoptionEvidenceRejection::AttemptNeverExecuted,
                            "adoption evidence names a trial attempt that never executed: this node opened it but recorded no terminal trial fact, so it is not a measurement",
                        ));
                    }
                    // A comparison attempt is bound by the plan digest it was
                    // opened with. A command trial has no plan: what the node
                    // probed around its run is the environment digest. An
                    // attempt with neither carries no binding at all.
                    if facts.plan_digest.is_none() && facts.environment_digest.is_none() {
                        bail!("the trial attempt carries no bound plan or environment");
                    }
                    if facts.measurement.carries_measurement() {
                        measurement = Some(match measurement {
                            Some(AttemptMeasurement::Measured) => AttemptMeasurement::Measured,
                            _ => facts.measurement,
                        });
                    } else {
                        measurement.get_or_insert(facts.measurement);
                    }
                    resolved.push(json!({
                        "kind": "trial_attempt",
                        "attempt_id": attempt_id,
                        "verified": true,
                        "plan_digest": facts.plan_digest,
                        "environment_digest": facts.environment_digest,
                        "outcome": facts.outcome,
                        "measurement": facts.measurement.as_str(),
                        "receipts": facts.receipts,
                        "scored": facts.scored,
                        "planned_slots": facts.planned_slots,
                        "bound_unit_id": facts.unit_id,
                        "bound_revision": facts.revision,
                        "bound_artifact_digest": facts.artifact_digest,
                    }));
                }
                AdoptionEvidence::StoredObservation { digest } => {
                    if !Self::observation_bound(&tx, &stored.unit, digest)? {
                        return Err(AdoptionEvidenceRejected::refusal(
                            AdoptionEvidenceRejection::ObservationNotHeld,
                            "adoption evidence names an observation this node does not hold for this unit version",
                        ));
                    }
                    resolved.push(json!({
                        "kind": "stored_observation",
                        "digest": digest.0,
                        "verified": true,
                        "empirical": false,
                    }));
                }
            }
        }
        // The empirical flag says what the evidence can carry. An operator
        // statement, an observation, or a trial that recorded no measurement
        // records what was said or attempted; none of them claims that a
        // measurement happened.
        let carried = measurement.is_some_and(|measurement| measurement.carries_measurement());
        let empirical = carried && method.is_empirical();
        let adoption = json!({
            "decision": decision.as_str(),
            "method": method.as_str(),
            "evidence": resolved,
            "evidence_verified": resolved
                .iter()
                .all(|item| item.get("verified").and_then(Value::as_bool) == Some(true)),
            "empirical": empirical,
            "measurement": measurement.map(|measurement| measurement.as_str()),
            // The subject's own confidence, stated or unknown. It is never a
            // number this node invented and never a reading of the evidence.
            "confidence": confidence,
            "notes": notes,
            "recorded_at": recorded_at,
        });
        set_config_section(&mut stored.unit.config, "cognition", "adoption", adoption.clone());
        tx.execute(
            "INSERT INTO unit_version_history(unit_id,revision,artifact_digest,imported_at,data) SELECT unit_id,revision,artifact_digest,imported_at,data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, artifact_digest.0],
        )?;
        tx.execute(
            "INSERT INTO unit_versions(unit_id,revision,artifact_digest,imported_at,data) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET data=excluded.data,imported_at=excluded.imported_at",
            params![
                unit_id,
                revision,
                artifact_digest.0,
                stored.imported_at.clone(),
                serde_json::to_string(&stored)?
            ],
        )?;
        if let Some(enable) = enable {
            tx.execute(
                "UPDATE unit_current SET enabled=?2,updated_at=?3 WHERE unit_id=?1",
                params![unit_id, i64::from(enable), recorded_at],
            )?;
        }
        tx.commit()?;
        let facts = self.retained_unit_facts(unit_id)?;
        Ok(json!({
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact_digest,
            "adoption": adoption,
            "enabled": enable,
            "versions": facts,
        }))
    }

    /// Whether this node holds the observation bytes and binds them to this
    /// exact unit version.
    fn observation_bound(
        conn: &Connection,
        unit: &UnitDescriptor,
        digest: &Digest,
    ) -> Result<bool> {
        let bound = unit
            .config
            .pointer("/cognition/observations")
            .and_then(Value::as_array)
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry.get("digest").and_then(Value::as_str) == Some(digest.0.as_str())
                })
            });
        if !bound {
            return Ok(false);
        }
        let bytes: Option<Vec<u8>> = conn.query_row(
            "SELECT bytes FROM objects WHERE digest=?1",
            [&digest.0],
            |r| r.get(0),
        ).optional()?;
        Ok(bytes.is_some_and(|bytes| Digest::blake3(&bytes) == *digest))
    }

    /// Register an observation about one unit version.
    ///
    /// The bytes are stored with their digest so the node can re-verify them,
    /// and they are bound to this exact unit version. An observation is a
    /// recorded fact; it never becomes empirical acceptance by itself.
    pub(super) fn unit_register_observation(
        &mut self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        kind: &str,
        bytes: &[u8],
    ) -> Result<Value> {
        let kind = kind.trim();
        ensure!(
            !kind.is_empty() && kind.len() <= MAX_TEXT_BYTES,
            "the observation kind is empty or too long"
        );
        ensure!(
            bytes.len() <= MAX_ARTIFACT_BYTES,
            "an observation exceeds the inline object bound"
        );
        let digest = Digest::blake3(bytes);
        let recorded_at = now();
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        let data: Option<String> = tx
            .query_row(
                "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit_id, revision, artifact_digest.0],
                |r| r.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            bail!("unit version is not retained on this node");
        };
        let mut stored: StoredVersion = serde_json::from_str(&data)?;
        tx.execute(
            "INSERT OR IGNORE INTO objects(digest,bytes) VALUES(?1,?2)",
            params![digest.0, bytes],
        )?;
        let mut entries = stored
            .unit
            .config
            .pointer("/cognition/observations")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        entries.retain(|entry| {
            entry.get("digest").and_then(Value::as_str) != Some(digest.0.as_str())
        });
        entries.push(json!({
            "digest": digest.0,
            "kind": kind,
            "bytes": bytes.len(),
            "recorded_at": recorded_at,
        }));
        set_config_section(
            &mut stored.unit.config,
            "cognition",
            "observations",
            Value::Array(entries.clone()),
        );
        tx.execute(
            "INSERT INTO unit_version_history(unit_id,revision,artifact_digest,imported_at,data) SELECT unit_id,revision,artifact_digest,imported_at,data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, artifact_digest.0],
        )?;
        tx.execute(
            "INSERT INTO unit_versions(unit_id,revision,artifact_digest,imported_at,data) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET data=excluded.data,imported_at=excluded.imported_at",
            params![
                unit_id,
                revision,
                artifact_digest.0,
                stored.imported_at.clone(),
                serde_json::to_string(&stored)?
            ],
        )?;
        tx.commit()?;
        Ok(json!({
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact_digest.0,
            "digest": digest.0,
            "kind": kind,
            "bytes": bytes.len(),
            "observations": entries,
        }))
    }

    /// The observations registered for one unit version.
    pub(super) fn unit_observations(&self, unit_id: &str) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let version = self
            .installed_unit_version(&unit.unit_id, &unit.revision, &unit.artifact_digest)?
            .context("the current unit version is not retained")?;
        let entries = version
            .descriptor
            .config
            .pointer("/cognition/observations")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let observations: Vec<Value> = entries
            .iter()
            .map(|entry| {
                let digest = entry
                    .get("digest")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let held = self
                    .object_bytes(&Digest(digest.clone()))
                    .map(|bytes| Digest::blake3(&bytes) == Digest(digest.clone()))
                    .unwrap_or(false);
                let mut entry = entry.clone();
                if let Some(object) = entry.as_object_mut() {
                    object.insert("bytes_held".into(), json!(held));
                }
                entry
            })
            .collect();
        Ok(json!({
            "unit_id": unit.unit_id,
            "revision": unit.revision,
            "artifact_digest": unit.artifact_digest.0,
            "observations": observations,
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::Node;
    use crate::units::UnitRequest;
    use crate::units::test_support::{import, package, scratch};
    use crate::units::trials::TrialOutcome;
    use serde_json::{Value, json};
    use super::{
        AdoptionDecision, AdoptionEvidence, AdoptionEvidenceRejected, AdoptionEvidenceRejection,
        AdoptionMethod,
    };
    /// Counterexample and admission proof for a trial that never ran.
    ///
    /// Nothing in this test dispatches a child. The attempts are opened
    /// against a real installation and closed — or deliberately left open —
    /// by the node itself, which is exactly the state a crash or a
    /// never-started trial leaves behind. A judgement citing an attempt that
    /// recorded no measurement must not become empirically adopted, and an
    /// attempt that was opened and never reached a terminal trial fact is not
    /// admissible evidence at all.
    #[test]
    fn orphaned_trial_cannot_certify_adoption_until_a_measurement_is_recorded() {
        let dir = scratch("adoption-evidence-node");
        let root = scratch("adoption-evidence-packages");
        let mut node = Node::open(&dir).unwrap();
        let material = package(
            "awareness.adoption-evidence",
            "1.0.0",
            b"a self-authored candidate nobody has measured\n",
            vec![],
        );
        let identity = material.unit.clone();
        import(&mut node, &root, vec![material]);

        let plan = json!({
            "unit_id": identity.unit_id,
            "revision": identity.revision,
            "artifact_digest": identity.artifact_digest,
            "candidate": {"enabled": true},
            "control": {"excluded_unit_id": identity.unit_id, "exclude_all_versions": true},
            "scenarios": [{"name": "self-test", "message_digest": "fixture", "rubric_digest": "fixture"}],
            "trials_per_condition": 1,
            "scope": "admission mechanics; no model or efficacy claim",
        });
        let begin = |node: &mut Node| -> String {
            node.handle_unit_request(UnitRequest::BeginTrial {
                unit_id: identity.unit_id.clone(),
                revision: identity.revision.clone(),
                artifact_digest: identity.artifact_digest.clone(),
                kind: "sandbox_comparison".into(),
                plan: plan.clone(),
            })
            .unwrap()["attempt_id"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let finish = |node: &mut Node, attempt: &str, outcome: TrialOutcome, receipts: Vec<Value>| {
            node.handle_unit_request(UnitRequest::FinishTrial {
                attempt_id: attempt.to_string(),
                outcome,
                receipts,
            })
        };
        let judge_with_confidence = |node: &mut Node, attempt: &str, confidence: Option<f64>| {
            node.handle_unit_request(UnitRequest::RecordAdoption {
                unit_id: identity.unit_id.clone(),
                revision: identity.revision.clone(),
                artifact_digest: identity.artifact_digest.clone(),
                decision: AdoptionDecision::Adopted,
                method: AdoptionMethod::SandboxComparison,
                evidence: vec![AdoptionEvidence::TrialAttempt {
                    attempt_id: attempt.to_string(),
                }],
                confidence,
                notes: None,
                enable: Some(true),
            })
        };
        let judge = |node: &mut Node, attempt: &str| {
            judge_with_confidence(node, attempt, None)
        };
        let sample = |condition: &str, trial: u64, verdict: &str, scenario: &str| {
            json!({
                "kind": "sample",
                "scenario": scenario,
                "condition": condition,
                "trial": trial,
                "verdict": verdict,
                "digest": identity.artifact_digest,
            })
        };

        // (1) Opened and never closed: no terminal trial fact exists, so the
        // citation is refused instead of being recorded as evidence.
        let orphan = begin(&mut node);
        let refused = judge(&mut node, &orphan).unwrap_err();
        assert_eq!(
            refused
                .downcast_ref::<AdoptionEvidenceRejected>()
                .map(|rejected| rejected.reason),
            Some(AdoptionEvidenceRejection::AttemptNeverExecuted),
            "an attempt that recorded no terminal fact was admitted as evidence: {refused:#}"
        );
        assert_eq!(
            AdoptionEvidenceRejection::AttemptNeverExecuted.code(),
            "evidence_unexecuted"
        );

        // (2) Opening another attempt retires the orphan as unknown — the
        // reopen path. The retired attempt is now a real record, so the
        // judgement is recorded, but nothing in it measured anything.
        let _live = begin(&mut node);
        let recorded = judge(&mut node, &orphan).unwrap();
        assert_eq!(recorded["adoption"]["empirical"], json!(false));
        assert_eq!(recorded["adoption"]["measurement"], json!("inconclusive"));
        assert_eq!(recorded["adoption"]["evidence"][0]["measurement"], json!("inconclusive"));
        assert_eq!(recorded["adoption"]["evidence"][0]["outcome"], json!("unknown"));
        assert_eq!(recorded["adoption"]["evidence"][0]["scored"], json!(0));

        // (3) A comparison closed before it measured: the real trial child
        // takes this path when the run failed before any sample was judged.
        let unmeasured = begin(&mut node);
        finish(&mut node, &unmeasured, TrialOutcome::Unknown, vec![]).unwrap();
        let recorded = judge(&mut node, &unmeasured).unwrap();
        assert_eq!(recorded["adoption"]["empirical"], json!(false));
        assert_eq!(recorded["adoption"]["measurement"], json!("inconclusive"));
        assert_eq!(recorded["adoption"]["evidence"][0]["receipts"], json!(0));

        // (4) A receipt that names a sample the plan never asked for was not
        // produced by this attempt's run, so closing with it is refused.
        let stray = begin(&mut node);
        let stray_finish = finish(
            &mut node,
            &stray,
            TrialOutcome::Succeeded,
            vec![sample("candidate", 0, "effective", "never-planned")],
        );
        assert!(
            stray_finish.is_err(),
            "a receipt outside the attempt's plan was accepted: {stray_finish:?}"
        );

        // (5) One of the two planned slots measured: a partial measurement is
        // still a measurement, and the record says it was partial.
        let partial = begin(&mut node);
        finish(
            &mut node,
            &partial,
            TrialOutcome::Unknown,
            vec![sample("candidate", 0, "effective", "self-test")],
        )
        .unwrap();
        let recorded = judge(&mut node, &partial).unwrap();
        assert_eq!(recorded["adoption"]["empirical"], json!(true));
        assert_eq!(recorded["adoption"]["measurement"], json!("partial"));
        assert_eq!(recorded["adoption"]["evidence"][0]["scored"], json!(1));
        assert_eq!(recorded["adoption"]["evidence"][0]["planned_slots"], json!(2));

        // (6) Every planned slot measured: the comparison is complete and the
        // judgement is empirical on a measured record.
        let measured = begin(&mut node);
        finish(
            &mut node,
            &measured,
            TrialOutcome::Failed,
            vec![
                sample("candidate", 0, "effective", "self-test"),
                sample("control", 0, "ineffective", "self-test"),
            ],
        )
        .unwrap();
        let recorded = judge_with_confidence(&mut node, &measured, Some(0.8)).unwrap();
        assert_eq!(recorded["adoption"]["empirical"], json!(true));
        assert_eq!(recorded["adoption"]["measurement"], json!("measured"));
        // The subject's own number is recorded as stated; a judgement that
        // states none records that explicitly instead of a filled-in number.
        assert_eq!(recorded["adoption"]["confidence"], json!(0.8));
        let unstated = judge(&mut node, &measured).unwrap();
        assert_eq!(unstated["adoption"]["confidence"], Value::Null);
        // A value that is not a number is refused; the node supplies no scale.
        let refused_confidence =
            judge_with_confidence(&mut node, &measured, Some(f64::NAN)).unwrap_err();
        assert!(
            format!("{refused_confidence:#}").contains("finite number"),
            "a non-number confidence was accepted: {refused_confidence:#}"
        );
        assert_eq!(recorded["adoption"]["evidence"][0]["outcome"], json!("failed"));
        assert_eq!(recorded["adoption"]["evidence"][0]["scored"], json!(2));

        // (7) A command trial that really ran is a measurement even when it
        // failed, while a command that never started is not one.
        let failed_run = node
            .handle_unit_request(UnitRequest::Trial {
                unit_id: identity.unit_id.clone(),
                argv: vec!["/bin/sh".into(), "-c".into(), "exit 3".into()],
                cwd: None,
                timeout_ms: Some(10_000),
                risk: "runs a trivial shell command".into(),
                benefit: "records a real negative execution fact".into(),
                consequence: "a failed command leaves the unit unverified here".into(),
            })
            .unwrap();
        assert_eq!(failed_run["outcome"], json!("failed"));
        let failed_attempt = failed_run["attempt_id"].as_str().unwrap().to_string();
        let recorded = judge(&mut node, &failed_attempt).unwrap();
        assert_eq!(
            recorded["adoption"]["measurement"],
            json!("measured"),
            "a real negative run was not counted as a measurement: {recorded}"
        );
        assert_eq!(recorded["adoption"]["empirical"], json!(true));

        let never_started = node
            .handle_unit_request(UnitRequest::Trial {
                unit_id: identity.unit_id.clone(),
                argv: vec!["/nonexistent/shenko-trial-program".into()],
                cwd: None,
                timeout_ms: Some(10_000),
                risk: "runs a program that is not installed".into(),
                benefit: "records that no execution happened".into(),
                consequence: "the unit stays unverified here".into(),
            })
            .unwrap();
        assert_eq!(never_started["outcome"], json!("failed"));
        let never_started_attempt = never_started["attempt_id"].as_str().unwrap().to_string();
        let recorded = judge(&mut node, &never_started_attempt).unwrap();
        assert_eq!(
            recorded["adoption"]["measurement"],
            json!("inconclusive"),
            "a command that never started was counted as a measurement: {recorded}"
        );
        assert_eq!(recorded["adoption"]["empirical"], json!(false));

        // The earlier judgements stay in the version's own history; each call
        // recorded one fact and never rewrote the version.
        assert_eq!(
            node.handle_unit_request(UnitRequest::Lineage { unit_id: identity.unit_id.clone() })
                .unwrap()["versions"][0]["revision"],
            json!(identity.revision)
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }
}
