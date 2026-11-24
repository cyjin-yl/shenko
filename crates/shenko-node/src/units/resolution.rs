//! Dependency selection, retained closure inspection and local readiness.
//!
//! Exact pins keep their recorded identity; only local artifact-bound process
//! evidence in the same environment can establish readiness.

use anyhow::{Context, Result, bail, ensure};
use crate::Node;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::object::Digest;
use std::cmp::Ordering;
use super::capabilities::{AtomicCapability, CapabilityContract};
use super::codec::UnitDescriptor;
use super::installation::{InstalledUnit, StoredVersion};
use super::materialization::materialization_matches;
use super::process::{TrialEvidence, evidence_workdir, probe_environment};
use super::trials::TrialOutcome;

/// Dependency strength is a separate axis from the matching mode.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyStrength {
    Required,
    Recommended,
}

/// Matching mode of a dependency edge. An exact selector names a unit and
/// pins at least one of revision or artifact digest; an abstract selector
/// names a capability or topic and is only satisfied by a unit that actually
/// declares providing it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "match", rename_all = "snake_case", deny_unknown_fields)]
pub enum DependencySelector {
    Exact {
        unit_id: String,
        revision_requirement: Option<String>,
        artifact_digest: Option<Digest>,
        /// The versioned contract the selected provider must declare, for an
        /// edge that also pins a service ABI or a code contract.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contract_requirement: Option<CapabilityContract>,
    },
    Abstract {
        capability_ref: Option<String>,
        topic_ref: Option<String>,
    },
}

/// One declared dependency edge of a unit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencySpec {
    pub edge_id: String,
    pub strength: DependencyStrength,
    pub selector: DependencySelector,
    pub purpose: Option<String>,
}

/// Resolution state of one declared edge.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeStatus {
    Satisfied,
    Omitted,
    Unresolved,
    /// Old rows cannot be attributed to this digest. Resolve this exact
    /// version or reimport it as an authored package to make a new decision.
    LegacyAmbiguous,
}

/// A resolved edge with the evidence for the choice.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResolvedDependency {
    pub edge_id: String,
    pub strength: DependencyStrength,
    pub selector: DependencySelector,
    pub status: EdgeStatus,
    pub selected_unit_id: Option<String>,
    pub selected_revision: Option<String>,
    pub selected_artifact_digest: Option<Digest>,
    /// The capability contract the selected provider was chosen for, when the
    /// declaration asked for one. A provider that changes contract major
    /// version stops satisfying the edge; its selection stays recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_contract: Option<CapabilityContract>,
    /// Other installed providers of the same abstract capability that coexist
    /// with the selection, when their actually used atomic capabilities do not
    /// conflict. They are enabled facts on this node, never a
    /// promise that they run here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coexisting_providers: Vec<ProviderSelection>,
    /// The atomic capabilities the selected provider declares using.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_capabilities: Vec<AtomicCapability>,
    pub reason: String,
}

/// Another provider of one abstract capability, as this node has it installed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderSelection {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<CapabilityContract>,
    pub enabled: bool,
}

/// Local readiness of an installed unit on this node.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessState {
    /// No unit is installed under this identity.
    NotInstalled,
    /// Installed and complete, but no local trial has proven it runs here.
    Unverified,
    /// A local trial of this exact artifact succeeded in this environment.
    Ready,
    /// A local trial of this exact artifact failed here.
    Failed,
    /// A local attempt ended without a known outcome; never replayed blindly.
    Unknown,
    /// A required dependency is unresolved or the package is incomplete.
    Unresolved,
    /// The bytes are not (fully) materialized in a package root yet.
    Unmaterialized,
}

#[derive(Default)]
pub(super) struct RequiredClosure {
    pub(super) unresolved: Vec<String>,
    pub(super) unmaterialized: Vec<String>,
    pub(super) root_materialized: bool,
}

pub(super) fn edge_matches_spec(edge: &ResolvedDependency, spec: &DependencySpec) -> bool {
    edge.edge_id == spec.edge_id
        && edge.strength == spec.strength
        && edge.selector == spec.selector
}

/// The exact selection a retained edge records, or `None` when the inventory
/// never resolved it. An unresolved or ambiguous edge is not a candidate for
/// an environment closure: guessing here would publish a closure the node
/// cannot explain.
pub(super) fn selection_of(edge: Option<&ResolvedDependency>) -> Option<(String, String, Digest)> {
    let edge = edge?;
    match (
        edge.status,
        edge.selected_unit_id.as_ref(),
        edge.selected_revision.as_ref(),
        edge.selected_artifact_digest.as_ref(),
    ) {
        (EdgeStatus::Satisfied, Some(unit_id), Some(revision), Some(digest)) => {
            Some((unit_id.clone(), revision.clone(), digest.clone()))
        }
        _ => None,
    }
}

pub(super) fn legacy_ambiguous(spec: &DependencySpec, reason: &str) -> ResolvedDependency {
    ResolvedDependency {
        edge_id: spec.edge_id.clone(),
        strength: spec.strength,
        selector: spec.selector.clone(),
        status: EdgeStatus::LegacyAmbiguous,
        selected_unit_id: None,
        selected_revision: None,
        selected_artifact_digest: None,
        selected_contract: None,
        coexisting_providers: Vec::new(),
        used_capabilities: Vec::new(),
        reason: reason.to_string(),
    }
}

pub(super) fn archive_edge(tx: &rusqlite::Transaction<'_>, unit_id: &str, revision: &str, digest: &str, edge_id: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO unit_edge_history(unit_id,revision,artifact_digest,edge_id,data) \
         SELECT unit_id,revision,artifact_digest,edge_id,data FROM unit_edges \
         WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 AND edge_id=?4",
        params![unit_id, revision, digest, edge_id],
    )?;
    Ok(())
}

/// One installed unit version a dependency declaration can resolve against.
///
/// A candidate carries what resolution actually decides on: the identity, the
/// declared `provides` entries (topic names and typed capability contracts).
/// Resource declarations belong to admission of the selected closure; eagerly
/// decoding every installed unit here would let an unrelated malformed Code
/// declaration prevent its own correction or any other package import.
#[derive(Clone, Debug)]
pub(super) struct Candidate {
    pub(super) unit_id: String,
    pub(super) revision: String,
    pub(super) digest: Digest,
    pub(super) provides: Vec<String>,
}

impl Candidate {
    fn of(descriptor: &UnitDescriptor) -> Self {
        Self {
            unit_id: descriptor.unit_id.clone(),
            revision: descriptor.revision.clone(),
            digest: descriptor.artifact_digest.clone(),
            provides: descriptor.provides.clone(),
        }
    }
}

/// Current versions come first; retained non-current versions remain valid
/// exact-digest candidates. This runs inside the import/resolution transaction.
pub(super) fn resolution_candidates(conn: &rusqlite::Connection) -> Result<Vec<Candidate>> {
    let current: Vec<(String, String, String, Option<String>)> = conn.prepare(
        "SELECT c.unit_id,c.revision,c.artifact_digest,v.data FROM unit_current AS c \
         LEFT JOIN unit_versions AS v ON v.unit_id=c.unit_id AND v.revision=c.revision AND v.artifact_digest=c.artifact_digest \
         ORDER BY c.unit_id"
    )?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let retained: Vec<(String, String, String, Option<String>)> = conn.prepare(
        "SELECT unit_id,revision,artifact_digest,data FROM unit_versions ORDER BY unit_id,revision,artifact_digest"
    )?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut available: Vec<Candidate> = Vec::new();
    for (unit_id, revision, digest, data) in current.into_iter().chain(retained) {
        let stored: StoredVersion = serde_json::from_str(&data.context("current unit version is not retained")?)?;
        ensure!(stored.unit.unit_id == unit_id && stored.unit.revision == revision && stored.unit.artifact_digest.0 == digest,
            "stored unit version identity does not match its key");
        if !available.iter().any(|candidate| candidate.unit_id == unit_id
            && candidate.revision == revision && candidate.digest == stored.unit.artifact_digest) {
            available.push(Candidate::of(&stored.unit));
        }
    }
    Ok(available)
}

/// A transferred pin is a historical selection, not permission to pick a
/// different candidate. If its target is absent, retain the reference and
/// retry only that target when it arrives.
pub(super) fn carry_edge(
    spec: &DependencySpec,
    edge: &ResolvedDependency,
    available: &[(String, String, Digest, Vec<String>)],
) -> Result<ResolvedDependency> {
    ensure!(edge_matches_spec(edge, spec), "dependency selection does not match its declaration");
    let contract = dependency_contract(spec)?;
    ensure!(edge.selected_contract.as_ref().is_none_or(|selected| Some(selected) == contract.as_ref()),
        "pinned dependency contract does not match its declaration");
    let selected = match (&edge.selected_unit_id, &edge.selected_revision, &edge.selected_artifact_digest) {
        (None, None, None) => None,
        (Some(id), Some(revision), Some(digest)) => Some((id, revision, digest)),
        _ => bail!("dependency selection has an incomplete pinned reference"),
    };
    ensure!(selected.is_some() || edge.selected_contract.is_none(),
        "unselected dependency cannot carry a selected contract");
    if selected.is_none() {
        ensure!(edge.status != EdgeStatus::Satisfied, "satisfied dependency has no selected reference");
        ensure!(edge.status != EdgeStatus::Omitted || spec.strength == DependencyStrength::Recommended,
            "required dependency cannot be omitted");
        return Ok(edge.clone());
    }
    ensure!(matches!(edge.status, EdgeStatus::Satisfied | EdgeStatus::Unresolved),
        "dependency selection conflicts with its status");
    let (id, revision, digest) = selected.unwrap();
    if let DependencySelector::Exact { unit_id, revision_requirement, artifact_digest, contract_requirement: _ } = &spec.selector {
        ensure!(id == unit_id
            && revision_requirement.as_ref().is_none_or(|requirement| revision_matches(requirement, revision))
            && artifact_digest.as_ref().is_none_or(|expected| expected == digest),
            "pinned dependency does not satisfy its exact selector");
    }
    let candidate = available.iter().find(|candidate| &candidate.0 == id && &candidate.1 == revision && &candidate.2 == digest);
    let mut result = edge.clone();
    if let Some(candidate) = candidate {
        let checked = resolve_edge(spec, std::slice::from_ref(candidate));
        if checked.status != EdgeStatus::Satisfied {
            result.status = EdgeStatus::Unresolved;
            result.reason = "the pinned dependency no longer satisfies its declaration; its exact selection is retained".into();
        } else if result.status != EdgeStatus::Satisfied {
            result.status = EdgeStatus::Satisfied;
            result.reason = checked.reason;
        }
    } else {
        result.status = EdgeStatus::Unresolved;
        result.reason = "the pinned dependency version is not retained; its selection is preserved until that exact version arrives".into();
    }
    Ok(result)
}

/// The contract explicitly requested by this declaration, never inferred from
/// whichever other services a selected package happens to provide. Opaque
/// content capabilities/topics retain their existing non-contract meaning.
fn dependency_contract(spec: &DependencySpec) -> Result<Option<CapabilityContract>> {
    match &spec.selector {
        DependencySelector::Exact { contract_requirement, .. } => Ok(contract_requirement.clone()),
        DependencySelector::Abstract { capability_ref: Some(reference), .. }
            if CapabilityContract::is_contract_ref(reference) => CapabilityContract::parse(reference).map(Some),
        _ => Ok(None),
    }
}

fn provides_contract(provides: &[String], contract: &CapabilityContract) -> bool {
    provides.iter().any(|reference| CapabilityContract::parse(reference)
        .is_ok_and(|provided| contract.matches(&provided)))
}

pub(super) fn selection_matches(spec: &DependencySpec, unit_id: &str, revision: &str, digest: &Digest, provides: &[String]) -> bool {
    match &spec.selector {
        DependencySelector::Exact { unit_id: expected, revision_requirement, artifact_digest, contract_requirement } => {
            unit_id == expected
                && revision_requirement.as_ref().is_none_or(|requirement| revision_matches(requirement, revision))
                && artifact_digest.as_ref().is_none_or(|expected| expected == digest)
                && contract_requirement.as_ref().is_none_or(|contract| provides_contract(provides, contract))
        }
        DependencySelector::Abstract { capability_ref, topic_ref } => {
            capability_ref.as_ref().is_none_or(|reference| provides.contains(reference))
                && topic_ref.as_ref().is_none_or(|reference| provides.contains(reference))
        }
    }
}

/// Resolve one declared edge against the available candidate set.
pub(super) fn resolve_edge(
    spec: &DependencySpec,
    available: &[(String, String, Digest, Vec<String>)],
) -> ResolvedDependency {
    let mut resolved = ResolvedDependency {
        edge_id: spec.edge_id.clone(),
        strength: spec.strength,
        selector: spec.selector.clone(),
        status: EdgeStatus::Unresolved,
        selected_unit_id: None,
        selected_revision: None,
        selected_artifact_digest: None,
        selected_contract: None,
        coexisting_providers: Vec::new(),
        used_capabilities: Vec::new(),
        reason: String::new(),
    };
    let contract = match dependency_contract(spec) {
        Ok(contract) => contract,
        Err(error) => {
            resolved.reason = format!("invalid requested capability contract: {error:#}");
            if spec.strength == DependencyStrength::Recommended { resolved.status = EdgeStatus::Omitted; }
            return resolved;
        }
    };
    match &spec.selector {
        DependencySelector::Exact { unit_id, revision_requirement, artifact_digest, contract_requirement: _ } => {
            let candidates: Vec<&(String, String, Digest, Vec<String>)> = available
                .iter()
                .filter(|candidate| &candidate.0 == unit_id)
                .collect();
            if candidates.is_empty() {
                resolved.reason = format!("no unit named {unit_id} is available here");
                if spec.strength == DependencyStrength::Recommended {
                    resolved.status = EdgeStatus::Omitted;
                    resolved.reason = format!("recommended dependency omitted: {}", resolved.reason);
                }
                return resolved;
            }
            let mut mismatch: Option<String> = None;
            for candidate in candidates {
                if let Some(requirement) = revision_requirement {
                    if !revision_matches(requirement, &candidate.1) {
                        mismatch = Some(format!(
                            "revision {} does not satisfy {requirement}",
                            candidate.1
                        ));
                        continue;
                    }
                }
                if let Some(expected) = artifact_digest {
                    if &candidate.2 != expected {
                        mismatch = Some("artifact digest does not match the pinned value".into());
                        continue;
                    }
                }
                if contract.as_ref().is_some_and(|contract| !provides_contract(&candidate.3, contract)) {
                    mismatch = Some("selected unit does not provide the required capability contract".into());
                    continue;
                }
                resolved.status = EdgeStatus::Satisfied;
                resolved.selected_unit_id = Some(candidate.0.clone());
                resolved.selected_revision = Some(candidate.1.clone());
                resolved.selected_artifact_digest = Some(candidate.2.clone());
                resolved.selected_contract = contract;
                resolved.reason =
                    format!("resolved to unit {} revision {}", candidate.0, candidate.1);
                return resolved;
            }
            resolved.reason =
                mismatch.unwrap_or_else(|| "no candidate satisfies the selector".into());
        }
        DependencySelector::Abstract {
            capability_ref,
            topic_ref,
        } => {
            for candidate in available {
                let capability_ok = match capability_ref {
                    Some(reference) => candidate.3.iter().any(|value| value == reference),
                    None => true,
                };
                let topic_ok = match topic_ref {
                    Some(reference) => candidate.3.iter().any(|value| value == reference),
                    None => true,
                };
                if capability_ok && topic_ok {
                    resolved.status = EdgeStatus::Satisfied;
                    resolved.selected_unit_id = Some(candidate.0.clone());
                    resolved.selected_revision = Some(candidate.1.clone());
                    resolved.selected_artifact_digest = Some(candidate.2.clone());
                    resolved.selected_contract = contract;
                    resolved.reason = format!(
                        "unit {} declares providing the requested capability or topic",
                        candidate.0
                    );
                    return resolved;
                }
            }
            resolved.reason =
                "no unit declares providing the requested capability or topic; similarity is not a match"
                    .to_string();
        }
    }
    if spec.strength == DependencyStrength::Recommended {
        // A missing recommendation is omitted, not a failure of the closure.
        resolved.status = EdgeStatus::Omitted;
        resolved.reason = format!("recommended dependency omitted: {}", resolved.reason);
    }
    resolved
}

/// Compare a revision against a simple requirement such as ">=1.0.0 <2.0.0".
pub(super) fn revision_matches(requirement: &str, revision: &str) -> bool {
    for part in requirement.split_whitespace() {
        let (operator, rest) = if let Some(rest) = part.strip_prefix(">=") {
            (">=", rest)
        } else if let Some(rest) = part.strip_prefix("<=") {
            ("<=", rest)
        } else if let Some(rest) = part.strip_prefix('>') {
            (">", rest)
        } else if let Some(rest) = part.strip_prefix('<') {
            ("<", rest)
        } else if let Some(rest) = part.strip_prefix('=') {
            ("=", rest)
        } else {
            ("=", part)
        };
        let order = compare_versions(revision, rest);
        let ok = match operator {
            ">=" => order != Ordering::Less,
            "<=" => order != Ordering::Greater,
            ">" => order == Ordering::Greater,
            "<" => order == Ordering::Less,
            _ => order == Ordering::Equal,
        };
        if !ok {
            return false;
        }
    }
    true
}

pub(super) fn compare_versions(left: &str, right: &str) -> Ordering {
    let left: Vec<&str> = left.split(['.', '-', '+']).collect();
    let right: Vec<&str> = right.split(['.', '-', '+']).collect();
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or("0");
        let b = right.get(index).copied().unwrap_or("0");
        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            (Ok(_), Err(_)) => Ordering::Greater,
            (Err(_), Ok(_)) => Ordering::Less,
            (Err(_), Err(_)) => a.cmp(b),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

pub(super) fn closure_value(edges: &[ResolvedDependency], closure: &RequiredClosure) -> Value {
    let complete = closure.unresolved.is_empty() && closure.unmaterialized.is_empty();
    json!({
        "state": if complete { "complete" } else { "incomplete" },
        "unresolved_required": closure.unresolved.len(),
        "unmaterialized_required": closure.unmaterialized.len(),
        "unresolved_details": &closure.unresolved,
        "unmaterialized_details": &closure.unmaterialized,
        "edges": edges,
    })
}

pub(super) fn unresolved_edges(edges: &[ResolvedDependency]) -> Value {
    json!(
        edges
            .iter()
            .filter(|edge| matches!(edge.status, EdgeStatus::Unresolved | EdgeStatus::LegacyAmbiguous))
            .collect::<Vec<_>>()
    )
}

/// Retained versions whose satisfied edge selects exactly this version.
///
/// An uninstall refuses to turn a satisfied closure into a gap. The list is
/// bounded and names the dependent and the edge, so an operator can retire the
/// dependent first instead of guessing why the withdrawal was refused.
pub(super) fn dependent_edges(
    conn: &rusqlite::Connection,
    unit_id: &str,
    revision: &str,
    digest: &Digest,
) -> Result<Vec<String>> {
    const MAX_REPORTED: usize = 16;
    let mut dependents = Vec::new();
    let mut statement = conn.prepare(
        "SELECT unit_id,revision,artifact_digest,edge_id,data FROM unit_edges \
         WHERE json_extract(data,'$.status')='satisfied' \
           AND json_extract(data,'$.selected_unit_id')=?1 \
           AND json_extract(data,'$.selected_revision')=?2 \
           AND json_extract(data,'$.selected_artifact_digest')=?3 \
         ORDER BY unit_id,revision,artifact_digest,edge_id",
    )?;
    let rows = statement.query_map(params![unit_id, revision, digest.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (dependent, dependent_revision, dependent_digest, edge_id, data) = row?;
        // The SQL filter narrows the scan; the parsed record still has to
        // agree, so a hand-edited or older row cannot be reported as a
        // dependent on a shape this code would not dispatch.
        let edge: ResolvedDependency = serde_json::from_str(&data)?;
        if edge.status != EdgeStatus::Satisfied
            || edge.selected_unit_id.as_deref() != Some(unit_id)
            || edge.selected_revision.as_deref() != Some(revision)
            || edge.selected_artifact_digest.as_ref() != Some(digest)
        {
            continue;
        }
        // The version's own outgoing edges do not depend on it.
        if dependent == unit_id && dependent_revision == revision && dependent_digest == digest.0 {
            continue;
        }
        dependents.push(format!("{dependent}@{dependent_revision}#{edge_id}"));
        if dependents.len() >= MAX_REPORTED {
            break;
        }
    }
    Ok(dependents)
}

impl Node {
    pub(super) fn unit_resolve_version(&mut self, unit_id: &str, revision: &str, artifact_digest: &Digest) -> Result<Value> {
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        let available: Vec<(String, String, Digest, Vec<String>)> = resolution_candidates(&tx)?
            .into_iter()
            .map(|c| (c.unit_id, c.revision, c.digest, c.provides))
            .collect();
        let data: Option<String> = tx.query_row(
            "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, artifact_digest.0], |r| r.get(0),
        ).optional()?;
        let stored: StoredVersion = serde_json::from_str(&data.context("unit version is not retained")?)?;
        ensure!(stored.unit.unit_id == unit_id && stored.unit.revision == revision && stored.unit.artifact_digest == *artifact_digest,
            "stored unit version identity does not match its key");
        for spec in &stored.unit.dependencies {
            let old_data: Option<String> = tx.query_row(
                "SELECT data FROM unit_edges WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 AND edge_id=?4",
                params![unit_id, revision, artifact_digest.0, spec.edge_id], |r| r.get(0),
            ).optional()?;
            let old: Option<ResolvedDependency> = old_data.as_deref().map(serde_json::from_str).transpose()?;
            let old = old.filter(|edge| edge_matches_spec(edge, spec)).unwrap_or_else(|| {
                legacy_ambiguous(spec, "no matching digest-bound edge is retained; make a new resolution for this version")
            });
            let updated = if old.status == EdgeStatus::Satisfied || old.selected_artifact_digest.is_some() {
                let mut updated = carry_edge(spec, &old, &available)?;
                if updated.status == EdgeStatus::Satisfied && updated.selected_contract.is_none() {
                    // This explicit repair may complete a legacy live edge's
                    // missing contract, but never choose another provider or
                    // rewrite a generation that already froze the old edge.
                    updated.selected_contract = dependency_contract(spec)?;
                    if updated.selected_contract.is_some() {
                        updated.reason.push_str("; explicit resolution recorded the declared contract for the retained exact selection");
                    }
                }
                updated
            } else {
                let candidate = resolve_edge(spec, &available);
                // A failed new attempt does not explain who owned the old row.
                if old.status == EdgeStatus::LegacyAmbiguous && candidate.status != EdgeStatus::Satisfied { old } else { candidate }
            };
            let data = serde_json::to_string(&updated)?;
            if old_data.as_ref().is_some_and(|old| old != &data) {
                archive_edge(&tx, unit_id, revision, &artifact_digest.0, &spec.edge_id)?;
            }
            tx.execute(
                "INSERT INTO unit_edges(unit_id,revision,artifact_digest,edge_id,data) VALUES(?1,?2,?3,?4,?5) \
                 ON CONFLICT(unit_id,revision,artifact_digest,edge_id) DO UPDATE SET data=excluded.data",
                params![unit_id, revision, artifact_digest.0, spec.edge_id, data],
            )?;
        }
        tx.commit()?;
        let unit = self.installed_unit_version(unit_id, revision, artifact_digest)?.context("unit version is not retained")?;
        let closure = self.required_closure(&unit)?;
        let current: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM unit_current WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3)",
            params![unit_id, revision, artifact_digest.0], |r| r.get(0),
        )?;
        Ok(json!({
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact_digest,
            "current": current,
            "closure": closure_value(&unit.edges, &closure),
            "note": "new resolution only; the current version and component activation were not changed",
        }))
    }

    /// Readiness of an installed unit on this node.
    ///
    /// Only local trial evidence of the exact artifact digest counts. Remote
    /// evidence and imported success never make a unit ready here.
    pub(super) fn readiness_of(&self, unit: &InstalledUnit) -> Result<(ReadinessState, String)> {
        let closure = self.required_closure(unit)?;
        self.readiness_with_closure(unit, &closure)
    }

    pub(super) fn required_closure(&self, root: &InstalledUnit) -> Result<RequiredClosure> {
        let root_materialized = materialization_matches(root);
        let mut closure = RequiredClosure {
            root_materialized,
            ..RequiredClosure::default()
        };
        if !root_materialized {
            closure.unmaterialized.push(format!(
                "{} revision {} does not have its selected artifact bytes",
                root.unit_id, root.revision
            ));
        }
        let mut visited = Vec::new();
        self.inspect_required_dependencies(root, &mut visited, &mut closure)?;
        Ok(closure)
    }

    fn inspect_required_dependencies(
        &self,
        unit: &InstalledUnit,
        visited: &mut Vec<(String, String, Digest)>,
        closure: &mut RequiredClosure,
    ) -> Result<()> {
        let current = (
            unit.unit_id.clone(),
            unit.revision.clone(),
            unit.artifact_digest.clone(),
        );
        if visited.contains(&current) {
            return Ok(());
        }
        visited.push(current);
        for edge in unit
            .edges
            .iter()
            .filter(|edge| edge.strength == DependencyStrength::Required)
        {
            if edge.status != EdgeStatus::Satisfied {
                closure.unresolved.push(format!(
                    "{} revision {} edge {}: {}",
                    unit.unit_id, unit.revision, edge.edge_id, edge.reason
                ));
                continue;
            }
            let selected = match (
                &edge.selected_unit_id,
                &edge.selected_revision,
                &edge.selected_artifact_digest,
            ) {
                (Some(unit_id), Some(revision), Some(digest)) => {
                    (unit_id.clone(), revision.clone(), digest.clone())
                }
                _ => {
                    closure.unresolved.push(format!(
                        "{} revision {} edge {} has an incomplete selected reference",
                        unit.unit_id, unit.revision, edge.edge_id
                    ));
                    continue;
                }
            };
            let Some(dependency) =
                self.installed_unit_version(&selected.0, &selected.1, &selected.2)?
            else {
                closure.unresolved.push(format!(
                    "{} revision {} edge {} selects a version that is not retained",
                    unit.unit_id, unit.revision, edge.edge_id
                ));
                continue;
            };
            let spec = unit.descriptor.dependencies.iter().find(|spec| edge_matches_spec(edge, spec))
                .context("stored dependency edge has no matching declaration")?;
            if !selection_matches(spec, &dependency.unit_id, &dependency.revision,
                &dependency.artifact_digest, &dependency.descriptor.provides) {
                closure.unresolved.push(format!(
                    "{} revision {} edge {} retains a selection that no longer satisfies its declaration",
                    unit.unit_id, unit.revision, edge.edge_id
                ));
                continue;
            }
            if visited.contains(&selected) {
                continue;
            }
            if !materialization_matches(&dependency) {
                closure.unmaterialized.push(format!(
                    "{} revision {} does not have its selected artifact bytes",
                    dependency.unit_id, dependency.revision
                ));
            }
            self.inspect_required_dependencies(&dependency, visited, closure)?;
        }
        Ok(())
    }

    pub(super) fn readiness_with_closure(
        &self,
        unit: &InstalledUnit,
        closure: &RequiredClosure,
    ) -> Result<(ReadinessState, String)> {
        if let Some(problem) = closure.unresolved.first() {
            return Ok((
                ReadinessState::Unresolved,
                format!("a required dependency is unresolved on this node: {problem}"),
            ));
        }
        if !closure.root_materialized {
            let detail = unit
                .materialization
                .as_ref()
                .map(|m| m.detail.clone())
                .unwrap_or_default();
            return Ok((
                ReadinessState::Unmaterialized,
                if detail.is_empty() {
                    "the current package bytes are missing or do not match the installed artifact digest"
                        .to_string()
                } else {
                    format!("the package bytes are not materialized: {detail}")
                },
            ));
        }
        if let Some(problem) = closure.unmaterialized.first() {
            return Ok((
                ReadinessState::Unmaterialized,
                format!(
                    "a required dependency is not materialized with its selected bytes: {problem}"
                ),
            ));
        }
        // Only a trial that ran the materialized artifact is a readiness
        // proof; an unbound investigation is evidence, not proof.
        let last: Option<(String, String, i64, String)> = self
            .conn
            .query_row(
                "SELECT environment_digest,outcome,artifact_verified,data FROM unit_trials \
                 WHERE unit_id=?1 AND artifact_digest=?2 AND artifact_bound=1 ORDER BY seq DESC LIMIT 1",
                params![unit.unit_id, unit.artifact_digest.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((environment_digest, outcome, artifact_verified, data)) = last else {
            let investigations: i64 = self
                .conn
                .query_row(
                    "SELECT count(*) FROM unit_trials WHERE unit_id=?1 AND artifact_digest=?2 AND artifact_bound=0",
                    params![unit.unit_id, unit.artifact_digest.0],
                    |r| r.get(0),
                )?;
            return Ok((
                ReadinessState::Unverified,
                if investigations > 0 {
                    "only unbound investigation commands have run here; they are not a readiness proof"
                        .to_string()
                } else {
                    "no local trial of the materialized artifact has run on this node yet"
                        .to_string()
                },
            ));
        };
        let evidence: TrialEvidence = match serde_json::from_str(&data) {
            Ok(value) => value,
            Err(_) => {
                return Ok((
                    ReadinessState::Unknown,
                    "the last local trial evidence could not be read".into(),
                ));
            }
        };
        // The environment is part of the proof: a changed interpreter, OS or
        // architecture does not inherit the previous result.
        let Some(workdir) = evidence_workdir(&evidence) else {
            return Ok((
                ReadinessState::Unverified,
                "the last local trial did not retain an absolute effective working directory"
                    .into(),
            ));
        };
        if !workdir.is_dir() {
            return Ok((
                ReadinessState::Unverified,
                "the last local trial's effective working directory is no longer available".into(),
            ));
        }
        let current = probe_environment(
            evidence.argv.first().map(|v| v.as_str()).unwrap_or(""),
            &workdir,
        )?;
        if current.digest != Digest(environment_digest) {
            return Ok((
                ReadinessState::Unverified,
                "the environment changed since the last local trial; the previous result does not carry over"
                    .into(),
            ));
        }
        let outcome = TrialOutcome::parse(&outcome)?;
        Ok(match outcome {
            TrialOutcome::Succeeded if artifact_verified != 0 => (
                ReadinessState::Ready,
                "a local trial of the materialized artifact succeeded in this environment".into(),
            ),
            TrialOutcome::Succeeded => (
                ReadinessState::Failed,
                "the materialized artifact did not match the installed digest during the trial"
                    .into(),
            ),
            TrialOutcome::Failed | TrialOutcome::Signaled | TrialOutcome::TimedOut => (
                ReadinessState::Failed,
                format!("the last local trial ended as {}", outcome.as_str()),
            ),
            TrialOutcome::Unknown => (
                ReadinessState::Unknown,
                "the last local attempt has an unknown outcome; it is never replayed automatically"
                    .into(),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::Node;
    use crate::units::CapabilityContract;
    use crate::units::test_support::{exact, import, package, scratch};
    use serde_json::{Value, json};
    use shenko_types::object::Digest;
    use super::{DependencySelector, DependencySpec, DependencyStrength, EdgeStatus, carry_edge, resolve_edge};

    #[test]
    fn contract_selections_record_requested_major_and_reject_incompatible_pins() {
        let requested = CapabilityContract::parse("cap.fixture.process/1").unwrap();
        let incompatible = CapabilityContract::parse("cap.fixture.process/2").unwrap();
        let digest = Digest::blake3(b"contract-provider");
        let available = vec![("provider".into(), "1.0.0".into(), digest.clone(), vec![requested.text()])];
        for selector in [
            DependencySelector::Exact { unit_id: "provider".into(), revision_requirement: Some("1.0.0".into()),
                artifact_digest: Some(digest.clone()), contract_requirement: Some(requested.clone()) },
            DependencySelector::Abstract { capability_ref: Some(requested.text()), topic_ref: None },
        ] {
            let spec = DependencySpec { edge_id: "provider".into(), strength: DependencyStrength::Required,
                selector, purpose: None };
            let selected = resolve_edge(&spec, &available);
            assert_eq!(selected.status, EdgeStatus::Satisfied);
            assert_eq!(selected.selected_contract.as_ref(), Some(&requested));
            assert_eq!(carry_edge(&spec, &selected, &available).unwrap(), selected);
            let wrong_major = vec![("provider".into(), "1.0.0".into(), digest.clone(), vec![incompatible.text()])];
            assert_eq!(resolve_edge(&spec, &wrong_major).status, EdgeStatus::Unresolved);
            let mut conflicting = selected.clone(); conflicting.selected_contract = Some(incompatible.clone());
            assert!(carry_edge(&spec, &conflicting, &available).is_err());
            let mut legacy = selected; legacy.selected_contract = None;
            assert_eq!(carry_edge(&spec, &legacy, &available).unwrap().selected_contract, None,
                "a historical transfer must not invent the missing contract; use explicit ResolveVersion");
        }
    }

    #[test]
    fn digest_versions_keep_independent_edges_and_re_resolve() {
        let dir = scratch("digest-node");
        let root = scratch("digest-packages");
        let mut node = Node::open(&dir).unwrap();
        let first = package("root", "1.0.0", b"root-one", vec![exact("edge", "first", None)]);
        let second = package("root", "1.0.0", b"root-two", vec![exact("edge", "second", None)]);
        import(&mut node, &root, vec![first.clone()]);
        import(&mut node, &root, vec![second.clone()]);
        import(&mut node, &root, vec![package("first", "1.0.0", b"first", vec![])]);
        let first_version = node.installed_unit_version("root", "1.0.0", &first.unit.artifact_digest).unwrap().unwrap();
        let second_version = node.installed_unit_version("root", "1.0.0", &second.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(first_version.edges[0].status, EdgeStatus::Satisfied);
        assert_eq!(first_version.edges[0].selected_unit_id.as_deref(), Some("first"));
        assert_eq!(second_version.edges[0].status, EdgeStatus::Unresolved);
        assert!(node.required_closure(&first_version).unwrap().unresolved.is_empty());
        assert_eq!(node.required_closure(&second_version).unwrap().unresolved.len(), 1);
        assert_ne!(first_version.materialization.as_ref().unwrap().path, second_version.materialization.as_ref().unwrap().path);
        import(&mut node, &root, vec![package("second", "1.0.0", b"second", vec![])]);
        let resolved = node.installed_unit_version("root", "1.0.0", &second.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(resolved.edges[0].selected_unit_id.as_deref(), Some("second"));
        assert!(node.required_closure(&resolved).unwrap().unresolved.is_empty());
    }

    #[test]
    fn incoming_descriptor_supersedes_its_stale_retained_candidate() {
        let mut node = Node::open(&scratch("capability-node")).unwrap();
        let root = scratch("capability-packages");
        let mut provider = package("provider", "1.0.0", b"provider", vec![]);
        provider.unit.provides.push("capability/example".into());
        import(&mut node, &root, vec![provider.clone()]);
        let mut replacement = package("provider", "1.0.0", b"provider-two", vec![]);
        replacement.unit.provides.push("capability/other".into());
        import(&mut node, &root, vec![replacement]);

        // The same retained digest is supplied with a new declaration that no
        // longer provides the capability, together with a new consumer.
        provider.unit.provides.clear();
        let consumer = package("consumer", "1.0.0", b"consumer", vec![DependencySpec {
            edge_id: "provider".into(), strength: DependencyStrength::Required,
            selector: DependencySelector::Abstract { capability_ref: Some("capability/example".into()), topic_ref: None },
            purpose: None,
        }]);
        import(&mut node, &root, vec![consumer, provider]);
        let status = node.unit_status("consumer").unwrap();
        assert_eq!(status["closure"]["state"], json!("incomplete"));
        assert_eq!(status["closure"]["edges"][0]["status"], json!("unresolved"));
        assert_eq!(status["closure"]["edges"][0]["selected_artifact_digest"], Value::Null);
        let mut valid = package("provider-new", "1.0.0", b"provider-new", vec![]);
        valid.unit.provides.push("capability/example".into());
        import(&mut node, &root, vec![valid]);
        assert_eq!(node.unit_status("consumer").unwrap()["closure"]["state"], json!("complete"));
        assert_eq!(node.unit_status("consumer").unwrap()["closure"]["edges"][0]["selected_unit_id"], json!("provider-new"));
    }
}
