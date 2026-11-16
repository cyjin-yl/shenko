//! Installation inventory and read projections over the owning `Node::conn`.
//!
//! Stored versions and current selections remain the single installation
//! authority shared by transfer, environment planning and trial evidence.

use anyhow::{Context, Result, ensure};
use crate::Node;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::object::Digest;
use super::codec::{
    Applicability, UnitArtifactSource, UnitDescriptor, UnitKind, b64_encode,
    external_artifact_of,
};
use super::environment::{EnvironmentMember, EnvironmentPin, read_environment_selector};
use super::materialization::{Materialization, MaterializationFact};
use super::resolution::{ResolvedDependency, closure_value, edge_matches_spec, legacy_ambiguous};

// --------------------------------------------------------------- records ---

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredVersion {
    pub(super) unit: UnitDescriptor,
    /// Evidence produced on the origin node. It is retained as provenance and
    /// never counts as local readiness. Values also preserve schema-1 records
    /// whose raw captures remain node-local and are never projected on export.
    pub(super) remote_evidence: Vec<Value>,
    pub(super) imported_at: String,
    pub(super) origin_node: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct InstalledUnit {
    pub(super) unit_id: String,
    pub(super) revision: String,
    pub(super) artifact_digest: Digest,
    pub(super) enabled: bool,
    pub(super) descriptor: UnitDescriptor,
    pub(super) edges: Vec<ResolvedDependency>,
    pub(super) materialization: Option<Materialization>,
}

/// One version another version declares it replaces.
///
/// The relation is declared by the replacing version, so it travels with
/// the package and keeps its reason. It is never guessed from wall-clock
/// order or from the number of copies a node holds.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SupersededVersion {
    pub revision: String,
    pub artifact_digest: Digest,
    pub reason: Option<String>,
}

/// The public, durable facts of one installed unit version.
///
/// This is the single installation authority for perception, awareness,
/// Skill and code units: version, enable state, dependencies, supersession
/// and the authored configuration. There is no second registry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledUnitFact {
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    pub artifact_digest: Digest,
    /// Whether this version is the one this node currently offers.
    pub current: bool,
    /// Whether this version is enabled for use.
    pub enabled: bool,
    pub imported_at: String,
    pub origin_node: Option<String>,
    pub descriptor: UnitDescriptor,
    pub edges: Vec<ResolvedDependency>,
    /// Versions this version declares it replaces.
    pub supersedes: Vec<SupersededVersion>,
    /// Retained versions that declare they replace this version.
    pub superseded_by: Vec<SupersededVersion>,
    /// Where the artifact bytes live.
    pub artifact_source: UnitArtifactSource,
    /// The materialization record, when the bytes were copied into a
    /// package root on this node.
    pub materialization: Option<MaterializationFact>,
}

/// The exact content of one installed unit version.
///
/// `bytes` are the artifact bytes whose digest is `artifact_digest`; the
/// caller verifies the digest when it matters.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitContent {
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    pub artifact_digest: Digest,
    pub enabled: bool,
    pub current: bool,
    pub bytes: Vec<u8>,
}

/// Public read-only projection of an installed unit that declares itself a
/// named tool (`config.tool` manifest). Carries only durable unit facts; the
/// artifact bytes are fetched separately through `Node::unit_object_bytes`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolUnitFact {
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    pub artifact_digest: Digest,
    pub enabled: bool,
    /// True when the installation's unit inventory currently names this
    /// version for its unit id (`unit_current`). It is a fact about the
    /// inventory, not about any environment generation.
    pub current: bool,
    /// True when the version this installation resolves for name-only use is
    /// this one: the active environment generation's pin when a generation is
    /// active, otherwise the `current` row. A version retained outside the
    /// active generation stays resolvable by exact name@version but is not
    /// selected.
    pub selected: bool,
    /// The active environment generation, when this version is its pin.
    pub environment: Option<EnvironmentPin>,
    /// The unit-authored `config.tool` object: `{name, abi, entrypoint,
    /// source, imports}` — recorded claims whose executable part
    /// (`abi`/`imports`) was verified at admission.
    pub tool: Value,
    /// Root this node's materialization was written to (e.g. `<data>/tools`).
    pub materialization_root: Option<String>,
    /// Exact materialized artifact path, when materialization completed.
    pub materialization_path: Option<String>,
}

pub(super) fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Set one key inside a named section of a unit's own configuration.
///
/// The configuration is authored by the package; this helper only adds a
/// node-recorded fact under its own section and never rewrites an authored
/// value.
pub(super) fn set_config_section(config: &mut Value, section: &str, key: &str, value: Value) {
    if !config.is_object() {
        *config = json!({});
    }
    let root = config.as_object_mut().expect("config is an object");
    let slot = root.entry(section.to_string()).or_insert_with(|| json!({}));
    if !slot.is_object() {
        *slot = json!({});
    }
    slot.as_object_mut()
        .expect("section is an object")
        .insert(key.to_string(), value);
}

/// The versions a unit version declares it replaces.
///
/// The relation is declared, never inferred: a node that holds two
/// versions of a unit does not decide by itself which one replaced which.
fn declared_supersedes(descriptor: &UnitDescriptor) -> Vec<SupersededVersion> {
    let Some(Value::Array(items)) = descriptor.config.pointer("/cognition/supersedes") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let revision = item.get("revision")?.as_str()?.to_string();
            let artifact_digest = Digest(item.get("artifact_digest")?.as_str()?.to_string());
            let reason = item
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(SupersededVersion {
                revision,
                artifact_digest,
                reason,
            })
        })
        .collect()
}

fn advisory_notes(applicability: Option<&Applicability>) -> Value {
    let mut notes: Vec<String> = Vec::new();
    match applicability {
        None => notes.push(
            "no applicability was declared; unknown labels stay advisory and never block a trial"
                .to_string(),
        ),
        Some(labels) => {
            match &labels.os {
                Some(os) if os != std::env::consts::OS => notes.push(format!(
                    "declared operating system {os} differs from this node's {}; the label is advisory and a trial is still permitted",
                    std::env::consts::OS
                )),
                Some(_) => notes.push("the declared operating system matches this node".to_string()),
                None => notes.push(
                    "no operating system was declared; the missing label is advisory and a trial is still permitted"
                        .to_string(),
                ),
            }
            match &labels.arch {
                Some(arch) if arch != std::env::consts::ARCH => notes.push(format!(
                    "declared architecture {arch} differs from this node's {}; the label is advisory and a trial is still permitted",
                    std::env::consts::ARCH
                )),
                Some(_) => notes.push("the declared architecture matches this node".to_string()),
                None => notes.push(
                    "no architecture was declared; the missing label is advisory and a trial is still permitted"
                        .to_string(),
                ),
            }
            if let Some(runtime) = &labels.runtime {
                notes.push(format!(
                    "declared runtime {runtime} is a claim to check by trial, not a verified fact"
                ));
            }
        }
    }
    json!(notes)
}

/// Remote evidence retained for provenance. It never counts as local
/// readiness.
pub(super) fn unit_remote_evidence(conn: &rusqlite::Connection, unit: &InstalledUnit) -> Result<Vec<Value>> {
    let data: String = conn.query_row(
        "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
        params![unit.unit_id, unit.revision, unit.artifact_digest.0],
        |r| r.get(0),
    )?;
    let stored: StoredVersion = serde_json::from_str(&data)?;
    Ok(stored.remote_evidence)
}

/// Read a list of strings from a stored value.
pub(super) fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|text| text.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

impl Node {
    // ------------------------------------------------------------ facts ----

    pub(super) fn unit_status(&self, unit_id: &str) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        Ok(self.status_value(&unit)?)
    }

    pub(super) fn unit_list(&self) -> Result<Value> {
        let mut units = Vec::new();
        for unit in self.installed_units()? {
            units.push(self.status_value(&unit)?);
        }
        Ok(json!({
            "units": units,
            "readiness_basis": "local_trials_only",
        }))
    }

    fn status_value(&self, unit: &InstalledUnit) -> Result<Value> {
        let closure = self.required_closure(unit)?;
        let (readiness, reason) = self.readiness_with_closure(unit, &closure)?;
        let local_trials = self.local_trial_count(&unit.unit_id, &unit.artifact_digest)?;
        let remote_evidence = unit_remote_evidence(&self.conn, unit)?.len() as u32;
        Ok(json!({
            "unit_id": unit.unit_id,
            "kind": unit.descriptor.kind,
            "revision": unit.revision,
            "artifact_digest": unit.artifact_digest,
            "installed": true,
            "enabled": unit.enabled,
            "materialized": closure.root_materialized,
            "package_root": unit.materialization.as_ref().map(|m| m.root.clone()).unwrap_or_default(),
            "package_path": unit.materialization.as_ref().map(|m| m.path.clone()).unwrap_or_default(),
            "materialization_detail": unit.materialization.as_ref().map(|m| m.detail.clone()).unwrap_or_default(),
            "provides": unit.descriptor.provides,
            "config": unit.descriptor.config,
            "applicability": unit.descriptor.applicability,
            "advisory": advisory_notes(unit.descriptor.applicability.as_ref()),
            "closure": closure_value(&unit.edges, &closure),
            "readiness": {
                "state": readiness,
                "reason": reason,
                "basis": "local_trials_only",
            },
            "local_trials": local_trials,
            "remote_evidence": remote_evidence,
        }))
    }

    pub(super) fn unit_set_enabled(&mut self, unit_id: &str, enabled: bool) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE unit_current SET enabled=?2,updated_at=?3 WHERE unit_id=?1",
            params![unit.unit_id, i64::from(enabled), now()],
        )?;
        tx.commit()?;
        let (readiness, reason) = self.readiness_of(&unit)?;
        Ok(json!({
            "unit_id": unit.unit_id,
            "enabled": enabled,
            "readiness": readiness,
            "reason": reason,
        }))
    }

    /// Read-only facts of every installed unit carrying a `tool` manifest in
    /// its declared configuration (`kind` is `Code`). Used by the named-WASM
    /// tool surface (`shenko tool`, `wasi.exec`): installation authority,
    /// versioning and replication stay here; the tool layer adds name/ABI
    /// resolution on top of these facts. Pure query — no schema creation, no
    /// trial reconciliation, no materialization.
    ///
    /// `selected` is the only field a name-only lookup may use. When an
    /// environment generation is active it is that generation's pin; when no
    /// generation exists yet the installation is honestly unpinned and the
    /// unit-current row is the selection.
    pub fn tool_unit_facts(&self) -> Result<Vec<ToolUnitFact>> {
        let selector = read_environment_selector(&self.conn)?;
        let pins: Vec<EnvironmentMember> = match &selector {
            Some(selector) => self.environment_members(selector.generation)?,
            None => Vec::new(),
        };
        let rows: Vec<(String, bool, bool, Option<String>, Option<String>)> = self
            .conn
            .prepare(
                "SELECT v.data,c.enabled,(c.revision=v.revision AND c.artifact_digest=v.artifact_digest),m.root,m.path \
                 FROM unit_versions v JOIN unit_current c ON c.unit_id=v.unit_id \
                 LEFT JOIN unit_materializations m ON m.unit_id=v.unit_id AND m.revision=v.revision AND m.artifact_digest=v.artifact_digest \
                 ORDER BY v.unit_id,v.revision,v.artifact_digest"
            )?
            .query_map([], |r| Ok((
                r.get::<_, String>(0)?, r.get::<_, bool>(1)?, r.get::<_, bool>(2)?,
                r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?,
            )))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut facts = Vec::new();
        for (data, enabled, current, root, path) in rows {
            let stored: StoredVersion = serde_json::from_str(&data)?;
            if stored.unit.kind != UnitKind::Code {
                continue;
            }
            let Some(tool) = stored.unit.config.get("tool").cloned() else {
                continue;
            };
            let pin = match &selector {
                Some(selector) => pins
                    .iter()
                    .find(|member| {
                        member.unit_id == stored.unit.unit_id
                            && member.revision == stored.unit.revision
                            && member.artifact_digest == stored.unit.artifact_digest
                    })
                    .map(|member| EnvironmentPin {
                        generation: selector.generation,
                        generation_digest: selector.generation_digest.clone(),
                        selector_revision: selector.revision,
                        role: member.role,
                        declarations: member.declarations.clone(),
                    }),
                None => None,
            };
            let selected = if selector.is_some() { pin.is_some() } else { current };
            facts.push(ToolUnitFact {
                unit_id: stored.unit.unit_id,
                kind: stored.unit.kind,
                revision: stored.unit.revision,
                artifact_digest: stored.unit.artifact_digest,
                enabled,
                current,
                selected,
                environment: pin,
                tool,
                materialization_root: root,
                materialization_path: path,
            });
        }
        Ok(facts)
    }

    /// Read-only access to a retained package's exact bytes, digest-verified.
    /// This is the authoritative artifact read for code-unit execution; the
    /// materialized file is a copy for transparency, not the trust root.
    pub fn unit_object_bytes(&self, digest: &Digest) -> Result<Vec<u8>> {
        self.object_bytes(digest)
    }

    // ------------------------------------------------- installation facts ---

    /// The current version of one unit, without enumerating unrelated units
    /// or loading every retained historical version.
    pub fn installed_unit_fact(&self, unit_id: &str) -> Result<Option<InstalledUnitFact>> {
        self.installed_unit(unit_id)?
            .as_ref()
            .map(|unit| self.unit_fact(unit, true))
            .transpose()
    }

    /// One exact retained version, with its actual current/enabled projection.
    pub fn retained_unit_fact(&self, unit_id: &str, revision: &str, digest: &Digest) -> Result<Option<InstalledUnitFact>> {
        let current = self.installed_unit(unit_id)?;
        if let Some(unit) = current.as_ref().filter(|unit| unit.revision == revision && unit.artifact_digest == *digest) {
            return self.unit_fact(unit, true).map(Some);
        }
        self.load_installed_unit(unit_id, revision, digest, false)?
            .as_ref()
            .map(|unit| self.unit_fact_with(unit, false, false))
            .transpose()
    }

    /// The durable facts of every unit this node currently offers.
    pub fn installed_unit_facts(&self) -> Result<Vec<InstalledUnitFact>> {
        let mut facts = Vec::new();
        for unit in self.installed_units()? {
            facts.push(self.unit_fact(&unit, true)?);
        }
        Ok(facts)
    }

    /// The durable facts of every retained version of one unit.
    ///
    /// Superseded versions stay visible with the relation that replaced
    /// them, so a revision history is a fact rather than a guess.
    pub fn retained_unit_facts(&self, unit_id: &str) -> Result<Vec<InstalledUnitFact>> {
        let current = self.installed_unit(unit_id)?;
        let mut facts = Vec::new();
        for unit in self.retained_unit_versions(unit_id)? {
            let is_current = current.as_ref().is_some_and(|current| {
                current.revision == unit.revision && current.artifact_digest == unit.artifact_digest
            });
            let enabled = is_current && current.as_ref().is_some_and(|current| current.enabled);
            facts.push(self.unit_fact_with(&unit, is_current, enabled)?);
        }
        Ok(facts)
    }

    /// The exact content of one installed unit version.
    ///
    /// A revision can have several retained artifact digests; the caller
    /// names one when the revision alone is ambiguous.
    pub fn unit_content(
        &self,
        unit_id: &str,
        revision: &str,
        artifact_digest: Option<&Digest>,
    ) -> Result<UnitContent> {
        let rows: Vec<(String, String)> = self
            .conn
            .prepare("SELECT revision,artifact_digest FROM unit_versions WHERE unit_id=?1 AND revision=?2 ORDER BY artifact_digest")?
            .query_map(params![unit_id, revision], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(!rows.is_empty(), "unit revision is not retained on this node");
        let selected = match artifact_digest {
            Some(digest) => rows
                .iter()
                .find(|(_, stored)| stored == &digest.0)
                .cloned()
                .context("unit revision does not retain the requested artifact digest")?,
            None => {
                ensure!(
                    rows.len() == 1,
                    "unit revision retains several artifacts; name the artifact digest"
                );
                rows[0].clone()
            }
        };
        let digest = Digest(selected.1);
        let unit = self
            .installed_unit_version(unit_id, revision, &digest)?
            .context("unit version is not retained on this node")?;
        let bytes = self.object_bytes(&digest)?;
        ensure!(
            Digest::blake3(&bytes) == digest,
            "stored artifact corruption"
        );
        let current = self.installed_unit(unit_id)?.is_some_and(|current| {
            current.revision == revision && current.artifact_digest == digest
        });
        Ok(UnitContent {
            unit_id: unit_id.to_string(),
            kind: unit.descriptor.kind,
            revision: revision.to_string(),
            artifact_digest: digest,
            enabled: unit.enabled,
            current,
            bytes,
        })
    }

    /// Where the bytes of one artifact actually live.
    ///
    /// An artifact registered out of line is not in this installation's
    /// inline object store; the returned locator says where it is, and the
    /// executor re-verifies the digest before it runs.
    pub fn unit_artifact_source(&self, digest: &Digest) -> Result<UnitArtifactSource> {
        let versions: Vec<String> = self
            .conn
            .prepare("SELECT data FROM unit_versions ORDER BY unit_id,revision,artifact_digest")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for data in versions {
            let stored: StoredVersion = serde_json::from_str(&data)?;
            if stored.unit.artifact_digest != *digest {
                continue;
            }
            if let Some(external) = external_artifact_of(&stored.unit) {
                return Ok(UnitArtifactSource::External {
                    store: external.store,
                    locator: external.locator,
                    bytes: external.bytes,
                });
            }
        }
        let retained: bool = self
            .conn
            .query_row("SELECT 1 FROM objects WHERE digest=?1", [&digest.0], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_some();
        ensure!(retained, "artifact is not retained on this node");
        Ok(UnitArtifactSource::Inline)
    }

    /// Read the exact content of one installed unit version.
    pub(super) fn unit_read(&self, unit_id: &str, revision: &str) -> Result<Value> {
        let content = self.unit_content(unit_id, revision, None)?;
        let text = String::from_utf8(content.bytes.clone()).ok();
        Ok(json!({
            "unit_id": content.unit_id,
            "kind": content.kind,
            "revision": content.revision,
            "artifact_digest": content.artifact_digest,
            "enabled": content.enabled,
            "current": content.current,
            "bytes": content.bytes.len(),
            "content_base64": b64_encode(&content.bytes),
            "content_text": text,
        }))
    }

    /// The retained versions of one unit and their supersession relations.
    pub(super) fn unit_lineage(&self, unit_id: &str) -> Result<Value> {
        let facts = self.retained_unit_facts(unit_id)?;
        let removed = self.removed_unit_facts(unit_id)?;
        ensure!(
            !facts.is_empty() || !removed.is_empty(),
            "unit is not installed on this node"
        );
        Ok(json!({
            "unit_id": unit_id,
            "current": facts
                .iter()
                .find(|fact| fact.current)
                .map(|fact| json!({
                    "revision": fact.revision,
                    "artifact_digest": fact.artifact_digest,
                    "enabled": fact.enabled,
                })),
            "versions": facts,
            "removed": removed,
        }))
    }

    /// Every retained version of one unit.
    pub(super) fn retained_unit_versions(&self, unit_id: &str) -> Result<Vec<InstalledUnit>> {
        let rows: Vec<(String, String)> = self
            .conn
            .prepare("SELECT revision,artifact_digest FROM unit_versions WHERE unit_id=?1 ORDER BY revision,artifact_digest")?
            .query_map(params![unit_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut units = Vec::new();
        for (revision, digest) in rows {
            if let Some(unit) = self.load_installed_unit(unit_id, &revision, &Digest(digest), false)? {
                units.push(unit);
            }
        }
        Ok(units)
    }

    /// The public projection of one installed version.
    fn unit_fact(&self, unit: &InstalledUnit, current: bool) -> Result<InstalledUnitFact> {
        self.unit_fact_with(unit, current, unit.enabled)
    }

    fn unit_fact_with(
        &self,
        unit: &InstalledUnit,
        current: bool,
        enabled: bool,
    ) -> Result<InstalledUnitFact> {
        let supersedes = declared_supersedes(&unit.descriptor);
        let mut superseded_by: Vec<SupersededVersion> = Vec::new();
        for other in self.retained_unit_versions(&unit.unit_id)? {
            if other.revision == unit.revision && other.artifact_digest == unit.artifact_digest {
                continue;
            }
            for relation in declared_supersedes(&other.descriptor) {
                if relation.revision == unit.revision && relation.artifact_digest == unit.artifact_digest {
                    superseded_by.push(SupersededVersion {
                        revision: other.revision.clone(),
                        artifact_digest: other.artifact_digest.clone(),
                        reason: relation.reason,
                    });
                }
            }
        }
        let artifact_source = match external_artifact_of(&unit.descriptor) {
            Some(external) => UnitArtifactSource::External {
                store: external.store,
                locator: external.locator,
                bytes: external.bytes,
            },
            None => UnitArtifactSource::Inline,
        };
        let imported_at = self
            .conn
            .query_row(
                "SELECT imported_at FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit.unit_id, unit.revision, unit.artifact_digest.0],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_default();
        let origin_node = self
            .conn
            .query_row(
                "SELECT json_extract(data,'$.origin_node') FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit.unit_id, unit.revision, unit.artifact_digest.0],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(InstalledUnitFact {
            unit_id: unit.unit_id.clone(),
            kind: unit.descriptor.kind,
            revision: unit.revision.clone(),
            artifact_digest: unit.artifact_digest.clone(),
            current,
            enabled,
            imported_at,
            origin_node,
            descriptor: unit.descriptor.clone(),
            edges: unit.edges.clone(),
            supersedes,
            superseded_by,
            artifact_source,
            materialization: unit.materialization.as_ref().map(|materialization| {
                MaterializationFact {
                    root: materialization.root.clone(),
                    path: materialization.path.clone(),
                    state: materialization.state.clone(),
                    detail: materialization.detail.clone(),
                }
            }),
        })
    }

    // ------------------------------------------------------- local facts ---

    pub(super) fn installed_units(&self) -> Result<Vec<InstalledUnit>> {
        let ids: Vec<String> = self
            .conn
            .prepare("SELECT unit_id FROM unit_current ORDER BY unit_id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut units = Vec::new();
        for id in ids {
            if let Some(unit) = self.installed_unit(&id)? {
                units.push(unit);
            }
        }
        Ok(units)
    }

    /// Capability names declared by currently enabled units on this node.
    /// These are declarations for coordination candidacy, not readiness or
    /// model-evidence claims. Reads on the caller's connection so a
    /// generation transaction can bind them to its own durable snapshot.
    pub(crate) fn enabled_provides(conn: &Connection) -> Result<Vec<String>> {
        let mut provides = Vec::new();
        let rows: Vec<(String, i64)> = conn
            .prepare(
                "SELECT v.data,c.enabled FROM unit_current c JOIN unit_versions v ON v.unit_id=c.unit_id AND v.revision=c.revision AND v.artifact_digest=c.artifact_digest ORDER BY c.unit_id",
            )?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (data, enabled) in rows {
            if enabled == 0 {
                continue;
            }
            let stored: StoredVersion = serde_json::from_str(&data)?;
            provides.extend(stored.unit.provides);
        }
        provides.sort();
        provides.dedup();
        Ok(provides)
    }


    pub(super) fn installed_unit(&self, unit_id: &str) -> Result<Option<InstalledUnit>> {
        let row: Option<(String, String, i64)> = self
            .conn
            .query_row(
                "SELECT revision,artifact_digest,enabled FROM unit_current WHERE unit_id=?1",
                [unit_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((revision, digest, enabled)) = row else {
            return Ok(None);
        };
        let unit = self
            .load_installed_unit(unit_id, &revision, &Digest(digest), enabled != 0)?
            .context("current unit version is not retained")?;
        Ok(Some(unit))
    }

    pub(super) fn installed_unit_version(
        &self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
    ) -> Result<Option<InstalledUnit>> {
        let enabled = self
            .conn
            .query_row(
                "SELECT enabled FROM unit_current WHERE unit_id=?1",
                [unit_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            != 0;
        self.load_installed_unit(unit_id, revision, artifact_digest, enabled)
    }

    fn load_installed_unit(
        &self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        enabled: bool,
    ) -> Result<Option<InstalledUnit>> {
        let data: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit_id, revision, artifact_digest.0],
                |r| r.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            return Ok(None);
        };
        let stored: StoredVersion = serde_json::from_str(&data)?;
        let stored_edges: Vec<ResolvedDependency> = self
            .conn
            .prepare(
                "SELECT data FROM unit_edges WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 ORDER BY edge_id",
            )?
            .query_map(params![unit_id, revision, artifact_digest.0], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|value| serde_json::from_str::<ResolvedDependency>(&value))
            .collect::<serde_json::Result<Vec<_>>>()?;
        // Old descriptors can be updated under the same artifact identity.
        // Stale edge rows must not become dependencies of the new descriptor,
        // and a missing row must never make its required closure look complete.
        let mut edges: Vec<ResolvedDependency> = stored.unit.dependencies.iter().map(|spec| {
            stored_edges.iter().find(|edge| edge_matches_spec(edge, spec)).cloned().unwrap_or_else(|| {
                legacy_ambiguous(spec, "no matching digest-bound edge is retained; use resolve_version for this exact version")
            })
        }).collect();
        edges.sort_by(|left, right| left.edge_id.cmp(&right.edge_id));
        let materialization: Option<Materialization> = self
            .conn
            .query_row(
                "SELECT root,path,state,detail FROM unit_materializations WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit_id, revision, artifact_digest.0],
                |r| {
                    Ok(Materialization {
                        root: r.get(0)?,
                        path: r.get(1)?,
                        state: r.get(2)?,
                        detail: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(Some(InstalledUnit {
            unit_id: unit_id.to_string(),
            revision: revision.to_string(),
            artifact_digest: artifact_digest.clone(),
            enabled,
            descriptor: stored.unit,
            edges,
            materialization,
        }))
    }

    pub(super) fn object_bytes(&self, digest: &Digest) -> Result<Vec<u8>> {
        let bytes: Vec<u8> = self.conn.query_row(
            "SELECT bytes FROM objects WHERE digest=?1",
            [&digest.0],
            |r| r.get(0),
        )?;
        ensure!(
            Digest::blake3(&bytes) == *digest,
            "stored object corruption"
        );
        Ok(bytes)
    }
}
