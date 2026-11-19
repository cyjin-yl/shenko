//! Atomic package import and portable export of retained dependency closures.
//!
//! Local adoption, observations, credentials and environment activation remain
//! local facts; imported trial provenance never establishes local readiness.

use anyhow::{Context, Result, bail, ensure};
use crate::Node;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use shenko_types::object::Digest;
use super::codec::{
    ExternalArtifact, MAX_ARTIFACT_BYTES, MAX_EXTERNAL_ARTIFACT_BYTES, MAX_PACKAGES,
    UNIT_SCHEMA, UnitDescriptor, UnitExport, UnitPackage, b64_decode, b64_encode,
    external_artifact_of, validate_descriptor,
};
use super::installation::{InstalledUnit, StoredVersion, now, set_config_section};
use super::materialization::normalize_root;
use super::resolution::{
    EdgeStatus, ResolvedDependency, archive_edge, carry_edge, closure_value, edge_matches_spec,
    legacy_ambiguous, resolution_candidates, resolve_edge, unresolved_edges,
};
use super::trials::TrialProvenance;

/// Remove the sections that are facts about one node only.
///
/// A recorded judgement and the observations bound to it describe what
/// happened on this installation. They are stripped from every package so a
/// peer never inherits an acceptance it did not measure.
fn strip_local_sections(config: &mut Value) {
    let empty = {
        let Some(section) = config.get_mut("cognition").and_then(Value::as_object_mut) else {
            return;
        };
        section.remove("adoption");
        section.remove("observations");
        section.is_empty()
    };
    if empty {
        if let Some(root) = config.as_object_mut() {
            root.remove("cognition");
        }
    }
}

impl Node {
    // ---------------------------------------------------------- import -----

    /// Register a transferred package closure in this installation.
    ///
    /// A package import writes unit versions, their edges, the inventory's
    /// per-unit current pointer, materialization state and trial provenance.
    /// It never writes `unit_environment.*`: the environment selector and the
    /// generations it names are local authority, so a `sync`/`receive` import
    /// (which is exactly this request) cannot activate a foreign environment.
    /// With a generation active, `tool_unit_facts` selects by that
    /// generation's pins, so even an import that changes `unit_current` does
    /// not change what a name-only resolution selects.
    pub(super) fn unit_import(
        &mut self,
        export: UnitExport,
        package_root: &str,
        enable: Option<bool>,
    ) -> Result<Value> {
        ensure!(matches!(export.schema, 2 | UNIT_SCHEMA), "unit export schema mismatch");
        ensure!(
            !export.packages.is_empty() && export.packages.len() <= MAX_PACKAGES,
            "unit package count out of bounds"
        );
        let root = normalize_root(package_root)?;
        // Verify every member before touching storage: a package with a wrong
        // digest is rejected as a whole.
        let mut members: Vec<(UnitDescriptor, Vec<u8>, Vec<TrialProvenance>, Option<Vec<ResolvedDependency>>)> = Vec::new();
        for package in &export.packages {
            let bytes = match external_artifact_of(&package.unit) {
                Some(external) => {
                    ensure!(
                        package.artifact_base64.is_empty(),
                        "an out-of-line artifact must not carry inline bytes"
                    );
                    ensure!(
                        external.digest == package.unit.artifact_digest,
                        "out-of-line artifact digest does not match the unit descriptor"
                    );
                    ensure!(
                        external.bytes <= MAX_EXTERNAL_ARTIFACT_BYTES,
                        "out-of-line artifact exceeds the size bound"
                    );
                    Vec::new()
                }
                None => {
                    let bytes = b64_decode(&package.artifact_base64)?;
                    ensure!(
                        bytes.len() <= MAX_ARTIFACT_BYTES,
                        "unit artifact exceeds the size bound"
                    );
                    ensure!(
                        Digest::blake3(&bytes) == package.unit.artifact_digest,
                        "unit artifact digest mismatch"
                    );
                    bytes
                }
            };
            ensure!(
                package.unit.artifact_digest.algorithm() == "blake3",
                "unsupported artifact digest algorithm"
            );
            validate_descriptor(&package.unit)?;
            ensure!(
                !members.iter().any(|(unit, _, _, _)| unit.unit_id == package.unit.unit_id
                    && unit.revision == package.unit.revision
                    && unit.artifact_digest == package.unit.artifact_digest),
                "duplicate unit version in package closure"
            );
            ensure!(export.schema != 2 || package.resolved_dependencies.is_none(), "schema-2 packages cannot carry selections");
            // The declared configuration is copied as the package authored it.
            // This module adds no gate on its content: actual local bindings
            // (environment values, secret files, provider grants) live outside
            // the unit configuration and are never read into a package.
            // A judgement recorded on another node and the observations bound
            // to it are local facts of that node: they never travel with the
            // package and never arrive as an acceptance here.
            let mut descriptor = package.unit.clone();
            strip_local_sections(&mut descriptor.config);
            members.push((descriptor, bytes, package.evidence.clone(), package.resolved_dependencies.clone()));
        }
        let root_index = match (&export.root_revision, &export.root_artifact_digest) {
            (Some(revision), Some(digest)) => {
                ensure!(export.schema == UNIT_SCHEMA, "schema-2 transfer cannot carry a digest-bound root");
                members.iter().position(|(unit, _, _, _)| unit.unit_id == export.root_unit_id
                    && &unit.revision == revision && &unit.artifact_digest == digest)
                    .context("selected root version is absent from the package closure")?
            }
            (None, None) => {
                let mut roots = members.iter().enumerate().filter(|(_, (unit, _, _, _))| unit.unit_id == export.root_unit_id);
                let first = roots.next().context("root unit is absent from the package closure")?.0;
                // Schema 2 exporters placed their root first. Schema 3 authored
                // closures with several root versions must identify one.
                ensure!(export.schema == 2 || roots.next().is_none(), "multiple root versions require a digest-bound root");
                first
            }
            _ => bail!("root version reference is incomplete"),
        };
        let imported_at = now();
        let resolutions: Vec<Vec<ResolvedDependency>>;
        {
            let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
            // Bind the candidates and existing pins to the same write-locked
            // database snapshot. Supplied versions take precedence only for
            // new selections, never for an already pinned local selection.
            let mut available = members.iter()
                .map(|(unit, _, _, _)| (unit.unit_id.clone(), unit.revision.clone(), unit.artifact_digest.clone(), unit.provides.clone()))
                .collect::<Vec<(String, String, Digest, Vec<String>)>>();
            // Incoming versions replace the exact same identity, not merely
            // take precedence. Their old retained descriptor would otherwise
            // keep declaring capabilities the package explicitly removed.
            let extra = resolution_candidates(&tx)?;
            for candidate in extra {
                let dominated = members.iter().any(|(unit, _, _, _)| unit.unit_id == candidate.unit_id
                    && unit.revision == candidate.revision && unit.artifact_digest == candidate.digest);
                if !dominated {
                    available.push((candidate.unit_id, candidate.revision, candidate.digest, candidate.provides));
                }
            }
            let mut resolved_versions = Vec::new();
            for (descriptor, _, _, carried) in &members {
                if let Some(carried) = carried {
                    ensure!(carried.len() == descriptor.dependencies.len(), "exported dependency selections do not match the declaration");
                    for edge in carried {
                        ensure!(descriptor.dependencies.iter().filter(|spec| edge_matches_spec(edge, spec)).count() == 1
                            && carried.iter().filter(|other| other.edge_id == edge.edge_id).count() == 1,
                            "exported dependency selection does not match the declaration");
                    }
                }
                let mut edges = Vec::new();
                for spec in &descriptor.dependencies {
                    let incoming = carried.as_ref().and_then(|edges| edges.iter().find(|edge| edge_matches_spec(edge, spec)));
                    let incoming = incoming.map(|edge| carry_edge(spec, edge, &available)).transpose()?;
                    let edge = if let Some(incoming) = incoming {
                        incoming
                    } else if export.schema == 2 {
                        legacy_ambiguous(spec, "schema-2 transfer did not carry a digest-bound dependency selection; use resolve_version for this exact version")
                    } else {
                        resolve_edge(spec, &available)
                    };
                    edges.push(edge);
                }
                resolved_versions.push(edges);
            }
            resolutions = resolved_versions;
            for (index, ((descriptor, bytes, evidence, carried), edges)) in
                members.iter().zip(resolutions.iter()).enumerate()
            {
                let mut descriptor = descriptor.clone();
                let old_version: Option<String> = tx.query_row(
                    "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                    params![descriptor.unit_id, descriptor.revision, descriptor.artifact_digest.0], |r| r.get(0),
                ).optional()?;
                let old_version: Option<StoredVersion> = old_version.map(|data| serde_json::from_str(&data)).transpose()?;
                // A recorded adoption is a fact about this installation, not
                // part of the authored package. Re-importing the same
                // artifact identity keeps the decision that was already
                // recorded instead of silently dropping it.
                if let Some(old) = old_version.as_ref() {
                    if let Some(adoption) = old.unit.config.pointer("/cognition/adoption").cloned() {
                        if descriptor.config.pointer("/cognition/adoption").is_none() {
                            set_config_section(&mut descriptor.config, "cognition", "adoption", adoption);
                        }
                    }
                    // The observations this installation recorded against
                    // this exact artifact identity are its own facts. A
                    // re-import of the same bytes must not drop them.
                    if let Some(observations) =
                        old.unit.config.pointer("/cognition/observations").cloned()
                    {
                        if descriptor.config.pointer("/cognition/observations").is_none() {
                            set_config_section(
                                &mut descriptor.config,
                                "cognition",
                                "observations",
                                observations,
                            );
                        }
                    }
                }
                // Package bytes reuse the existing inline object store.
                if external_artifact_of(&descriptor).is_none() {
                    let stored = crate::seal(&tx, bytes, "application/octet-stream", "unit-package")?;
                    ensure!(
                        stored.digest == descriptor.artifact_digest,
                        "stored artifact digest mismatch"
                    );
                }
                let record = StoredVersion {
                    unit: descriptor.clone(),
                    remote_evidence: evidence
                        .iter()
                        .map(serde_json::to_value)
                        .collect::<serde_json::Result<Vec<_>>>()?,
                    imported_at: imported_at.clone(),
                    origin_node: export.origin_node.clone(),
                };
                // Reimport may update the descriptor or remote provenance at
                // the same artifact identity. Retain the exact prior record.
                tx.execute(
                    "INSERT INTO unit_version_history(unit_id,revision,artifact_digest,imported_at,data) \
                     SELECT unit_id,revision,artifact_digest,imported_at,data FROM unit_versions \
                     WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                    params![descriptor.unit_id, descriptor.revision, descriptor.artifact_digest.0],
                )?;
                tx.execute(
                    "INSERT INTO unit_versions(unit_id,revision,artifact_digest,imported_at,data) \
                     VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET \
                     data=excluded.data,imported_at=excluded.imported_at",
                    params![
                        descriptor.unit_id,
                        descriptor.revision,
                        descriptor.artifact_digest.0,
                        imported_at,
                        serde_json::to_string(&record)?
                    ],
                )?;
                // The first package for an identity chooses its current
                // version; the declared root is explicitly first among root
                // versions. Other retained closure members do not overwrite it.
                let current_row: Option<(String, String, i64)> = tx.query_row(
                    "SELECT revision,artifact_digest,enabled FROM unit_current WHERE unit_id=?1",
                    params![descriptor.unit_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                ).optional()?;
                let already_held = old_version.is_some();
                let current_revision = current_row.as_ref().map(|row| row.0.clone()).unwrap_or_default();
                let current_digest = current_row.as_ref().map(|row| row.1.clone()).unwrap_or_default();
                let current_names_other = current_revision != descriptor.revision
                    || current_digest != descriptor.artifact_digest.0;
                let choose_current = (index == root_index || (descriptor.unit_id != export.root_unit_id
                    && !members[..index].iter().any(|(unit, _, _, _)| unit.unit_id == descriptor.unit_id)))
                    && !(already_held && current_names_other);
                if choose_current {
                    let previous_enabled = current_row.as_ref().map(|row| row.2 != 0).unwrap_or(false);
                    let enabled = if already_held {
                        previous_enabled
                    } else if index == root_index {
                        enable.unwrap_or(previous_enabled)
                    } else {
                        previous_enabled
                    };
                    tx.execute(
                    "INSERT INTO unit_current(unit_id,revision,artifact_digest,enabled,updated_at) \
                     VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id) DO UPDATE SET \
                     revision=excluded.revision,artifact_digest=excluded.artifact_digest,\
                     enabled=excluded.enabled,updated_at=excluded.updated_at",
                    params![
                        descriptor.unit_id,
                        descriptor.revision,
                        descriptor.artifact_digest.0,
                        i64::from(enabled),
                        imported_at
                    ],
                    )?;
                }
                for edge in edges {
                    let old_data: Option<String> = tx.query_row(
                        "SELECT data FROM unit_edges WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 AND edge_id=?4",
                        params![descriptor.unit_id, descriptor.revision, descriptor.artifact_digest.0, edge.edge_id], |r| r.get(0),
                    ).optional()?;
                    let old_edge: Option<ResolvedDependency> = old_data.as_deref().map(serde_json::from_str).transpose()?;
                    let old_pinned = old_edge.as_ref().filter(|old| {
                        old_version.as_ref().is_some_and(|version| version.unit.dependencies.iter().any(|spec| edge_matches_spec(old, spec)))
                            && old.edge_id == edge.edge_id && old.strength == edge.strength && old.selector == edge.selector
                            && (old.status == EdgeStatus::Satisfied || old.selected_unit_id.is_some()
                                || old.selected_revision.is_some() || old.selected_artifact_digest.is_some())
                    });
                    if let Some(old) = old_pinned {
                        if carried.is_some() && edge.selected_artifact_digest.is_some() {
                            ensure!(old.selected_unit_id == edge.selected_unit_id
                                && old.selected_revision == edge.selected_revision
                                && old.selected_artifact_digest == edge.selected_artifact_digest,
                                "conflicting digest-bound dependency selections; neither pinned choice was replaced");
                        }
                    }
                    let edge = old_pinned.unwrap_or(edge);
                    let data = serde_json::to_string(&edge)?;
                    if old_data.as_ref().is_some_and(|old| old != &data) {
                        archive_edge(&tx, &descriptor.unit_id, &descriptor.revision, &descriptor.artifact_digest.0, &edge.edge_id)?;
                    }
                    tx.execute(
                        "INSERT INTO unit_edges(unit_id,revision,artifact_digest,edge_id,data) VALUES(?1,?2,?3,?4,?5) \
                         ON CONFLICT(unit_id,revision,artifact_digest,edge_id) DO UPDATE SET data=excluded.data",
                        params![
                            descriptor.unit_id,
                            descriptor.revision,
                            descriptor.artifact_digest.0,
                            edge.edge_id,
                            data
                        ],
                    )?;
                }
                tx.execute(
                    "INSERT INTO unit_materializations(unit_id,revision,artifact_digest,root,path,state,detail,updated_at) \
                     VALUES(?1,?2,?3,'','','pending','',?4) \
                     ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET \
                     root='',path='',state='pending',detail='',updated_at=excluded.updated_at",
                    params![
                        descriptor.unit_id,
                        descriptor.revision,
                        descriptor.artifact_digest.0,
                        imported_at
                    ],
                )?;
            }
            // A later package may complete an older unit's required closure.
            // Keep successful exact selections pinned to their retained bytes;
            // only retry unresolved/omitted edges against this import's facts.
            let pending_edges = tx
                .prepare(
                    "SELECT e.unit_id,e.revision,e.artifact_digest,e.edge_id,e.data,v.data FROM unit_edges AS e \
                     JOIN unit_versions AS v ON v.unit_id=e.unit_id AND v.revision=e.revision AND v.artifact_digest=e.artifact_digest \
                     WHERE json_extract(e.data,'$.status') NOT IN ('satisfied','legacy_ambiguous')",
                )?
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (unit_id, revision, artifact_digest, edge_id, data, version_data) in pending_edges {
                let previous: ResolvedDependency = serde_json::from_str(&data)?;
                let stored: StoredVersion = serde_json::from_str(&version_data)?;
                let Some(spec) = stored.unit.dependencies.iter().find(|spec| edge_matches_spec(&previous, spec)) else { continue; };
                let updated = if previous.selected_artifact_digest.is_some() {
                    carry_edge(spec, &previous, &available)?
                } else { resolve_edge(spec, &available) };
                if updated != previous {
                    archive_edge(&tx, &unit_id, &revision, &artifact_digest, &edge_id)?;
                    tx.execute(
                        "UPDATE unit_edges SET data=?5 WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 AND edge_id=?4",
                        params![unit_id, revision, artifact_digest, edge_id, serde_json::to_string(&updated)?],
                    )?;
                }
            }
            tx.commit()?;
        }
        // Materialization happens outside the database transaction; a partial
        // materialization leaves the unit not ready and is retryable.
        let mut statuses = Vec::new();
        for (descriptor, _, _, _) in &members {
            let status = self.materialize_version(&descriptor.unit_id, &descriptor.revision, &descriptor.artifact_digest, &root)?;
            statuses.push(status);
        }
        let mut dependency_resolutions = Vec::new();
        for ((descriptor, _, _, _), offered) in members.iter().zip(resolutions.iter()) {
            let unit = self.installed_unit_version(&descriptor.unit_id, &descriptor.revision, &descriptor.artifact_digest)?
                .context("imported unit version is not retained")?;
            let closure = self.required_closure(&unit)?;
            let closure = closure_value(&unit.edges, &closure);
            dependency_resolutions.push(json!({
                "unit_id": descriptor.unit_id,
                "revision": descriptor.revision,
                "artifact_digest": descriptor.artifact_digest,
                "offered_edges": offered,
                "retained_edges": &unit.edges,
                "closure": closure,
            }));
        }
        Ok(json!({
            "imported": statuses.len(),
            "package_root": root,
            "readiness_basis": "local_trials_only",
            "note": "imported packages are not ready here; only a local trial can make them ready",
            "units": statuses,
            "dependency_resolutions": dependency_resolutions,
        }))
    }

    /// Typed export of every unit installed on this node.
    ///
    /// One entry per installed unit, each holding the unit itself and the
    /// dependency closure this node could resolve. The caller can import the
    /// result with `UnitRequest::Import` without a `serde_json::Value`
    /// roundtrip.
    pub fn export_installation_units(&self) -> Result<Vec<UnitExport>> {
        let mut exports = Vec::new();
        for unit in self.installed_units()? {
            exports.push(self.export_unit_closure(&unit)?);
        }
        Ok(exports)
    }

    /// Export a unit and the dependency closure this node can actually offer.
    pub(super) fn unit_export(&self, unit_id: &str) -> Result<Value> {
        let root = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let export = self.export_unit_closure(&root)?;
        let closure = self.required_closure(&root)?;
        Ok(json!({
            "export": export,
            "unresolved_edges": unresolved_edges(&root.edges),
            "closure": closure_value(&root.edges, &closure),
            "note": "exported evidence belongs to the origin node; importing it never makes the unit ready here",
        }))
    }

    /// Build the transferable closure of one installed unit.
    fn export_unit_closure(&self, root: &InstalledUnit) -> Result<UnitExport> {
        let mut packages: Vec<UnitPackage> = Vec::new();
        let mut seen: Vec<(String, String, Digest)> = Vec::new();
        let mut queue: Vec<(String, String, Digest)> = vec![(
            root.unit_id.clone(),
            root.revision.clone(),
            root.artifact_digest.clone(),
        )];
        while let Some(current) = queue.pop() {
            if seen.contains(&current) {
                continue;
            }
            ensure!(packages.len() < MAX_PACKAGES, "unit dependency closure exceeds the transferable package limit");
            seen.push(current.clone());
            let Some(unit) = self.installed_unit_version(&current.0, &current.1, &current.2)?
            else {
                continue;
            };
            // An out-of-line artifact is not re-inlined into an export: the
            // portable identity is its digest, the locator is
            // installation-local.
            let bytes = if external_artifact_of(&unit.descriptor).is_some() {
                Vec::new()
            } else {
                self.object_bytes(&unit.artifact_digest)?
            };
            // A judgement recorded on this node and the observations bound to
            // it are facts about this installation. They never travel with a
            // package: another node must reach its own conclusion.
            let mut descriptor = unit.descriptor.clone();
            strip_local_sections(&mut descriptor.config);
            packages.push(UnitPackage {
                unit: descriptor,
                artifact_base64: b64_encode(&bytes),
                evidence: self.local_provenance(&unit.unit_id, &unit.artifact_digest)?,
                resolved_dependencies: Some(unit.edges.clone()),
            });
            for edge in &unit.edges {
                if edge.status == EdgeStatus::Satisfied {
                    if let (Some(unit_id), Some(revision), Some(digest)) = (
                        &edge.selected_unit_id,
                        &edge.selected_revision,
                        &edge.selected_artifact_digest,
                    ) {
                        let selected = (unit_id.clone(), revision.clone(), digest.clone());
                        if !seen.contains(&selected) {
                            queue.push(selected);
                        }
                    }
                }
            }
        }
        let origin_node = self.local_node_id()?.map(|node| node.to_string());
        Ok(UnitExport {
            schema: UNIT_SCHEMA,
            root_unit_id: root.unit_id.clone(),
            root_revision: Some(root.revision.clone()),
            root_artifact_digest: Some(root.artifact_digest.clone()),
            origin_node,
            enabled: root.enabled,
            packages,
        })
    }

    /// Register a unit version whose artifact bytes live outside this
    /// installation's inline object store.
    ///
    /// The bytes are never copied into the node database. The declared
    /// digest is the portable identity: a peer receives it and registers the
    /// unit against its own bytes.
    pub(super) fn unit_import_external(
        &mut self,
        unit: UnitDescriptor,
        artifact: ExternalArtifact,
        package_root: &str,
        enable: Option<bool>,
    ) -> Result<Value> {
        ensure!(
            artifact.digest.algorithm() == "blake3",
            "unsupported artifact digest algorithm"
        );
        ensure!(
            artifact.digest == unit.artifact_digest,
            "external artifact digest does not match the unit descriptor"
        );
        ensure!(
            artifact.bytes <= MAX_EXTERNAL_ARTIFACT_BYTES,
            "external artifact exceeds the size bound"
        );
        ensure!(!artifact.locator.trim().is_empty(), "external artifact locator is empty");
        ensure!(!artifact.store.trim().is_empty(), "external artifact store is empty");
        validate_descriptor(&unit)?;
        let root = normalize_root(package_root)?;
        let mut descriptor = unit;
        descriptor.config["external_artifact"] = json!({
            "digest": artifact.digest,
            "bytes": artifact.bytes,
            "store": artifact.store,
            "locator": artifact.locator,
        });
        let export = UnitExport {
            schema: UNIT_SCHEMA,
            root_unit_id: descriptor.unit_id.clone(),
            root_revision: Some(descriptor.revision.clone()),
            root_artifact_digest: Some(descriptor.artifact_digest.clone()),
            origin_node: None,
            enabled: enable.unwrap_or(false),
            packages: vec![UnitPackage {
                unit: descriptor,
                artifact_base64: String::new(),
                evidence: vec![],
                resolved_dependencies: None,
            }],
        };
        let mut outcome = self.unit_import(export, &root, enable)?;
        if let Some(object) = outcome.as_object_mut() {
            object.insert(
                "artifact_source".into(),
                json!({
                    "source": "external",
                    "store": artifact.store,
                    "locator": artifact.locator,
                    "bytes": artifact.bytes,
                }),
            );
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use crate::Node;
    use crate::units::UnitRequest;
    use crate::units::codec::UnitExport;
    use crate::units::materialization::materialization_matches;
    use crate::units::resolution::{DependencySelector, DependencySpec, DependencyStrength, EdgeStatus};
    use crate::units::test_support::{exact, export, import, package, scratch, trial};
    use serde_json::json;
    use shenko_types::object::Digest;

    #[test]
    fn exported_root_digest_is_independent_of_package_order() {
        let mut source = Node::open(&scratch("root-binding-source")).unwrap();
        let source_root = scratch("root-binding-source-packages");
        let old = package("self", "1.0.0", b"self-old", vec![]);
        let new = package("self", "1.0.0", b"self-new", vec![
            exact("old", "self", Some(old.unit.artifact_digest.clone())),
        ]);
        import(&mut source, &source_root, vec![old.clone()]);
        import(&mut source, &source_root, vec![new.clone()]);
        let mut transfer = source.export_unit_closure(&source.installed_unit("self").unwrap().unwrap()).unwrap();
        assert_eq!(transfer.root_artifact_digest, Some(new.unit.artifact_digest.clone()));
        assert_eq!(transfer.packages.len(), 2);
        transfer.packages.reverse();
        let mut receiver = Node::open(&scratch("root-binding-receiver")).unwrap();
        let receiver_root = scratch("root-binding-receiver-packages");
        receiver.handle_unit_request(UnitRequest::Import {
            export: transfer, package_root: receiver_root.to_string_lossy().into_owned(), enable: None,
        }).unwrap();
        let status = receiver.unit_status("self").unwrap();
        assert_eq!(status["artifact_digest"], json!(&new.unit.artifact_digest.0));
        assert_eq!(status["closure"]["state"], json!("complete"));
        let retained = receiver.installed_unit_version("self", "1.0.0", &old.unit.artifact_digest).unwrap().unwrap();
        assert!(materialization_matches(&retained));
    }

    #[test]
    fn export_retains_distinct_pins_and_recovers_a_missing_selected_version() {
        let dir = scratch("pin-origin");
        let root = scratch("pin-origin-packages");
        let mut origin = Node::open(&dir).unwrap();
        let one = package("target", "1.0.0", b"target-one", vec![]);
        let two = package("target", "1.0.0", b"target-two", vec![]);
        let a = package("middle", "1.0.0", b"middle-one", vec![exact("target", "target", None)]);
        let b = package("middle", "1.0.0", b"middle-two", vec![exact("target", "target", None)]);
        import(&mut origin, &root, vec![one.clone()]);
        import(&mut origin, &root, vec![a.clone()]);
        import(&mut origin, &root, vec![two.clone()]);
        import(&mut origin, &root, vec![b.clone()]);
        let top = package("top", "1.0.0", b"top", vec![
            exact("a", "middle", Some(a.unit.artifact_digest.clone())),
            exact("b", "middle", Some(b.unit.artifact_digest.clone())),
        ]);
        import(&mut origin, &root, vec![top]);
        let transfer = origin.export_unit_closure(&origin.installed_unit("top").unwrap().unwrap()).unwrap();
        assert_eq!(transfer.packages.len(), 5);

        let mut incomplete = transfer;
        incomplete.packages.retain(|p| p.unit.artifact_digest != one.unit.artifact_digest);
        let mut recovering = Node::open(&scratch("pin-recovery")).unwrap();
        let recovery_root = scratch("pin-recovery-packages");
        recovering.handle_unit_request(UnitRequest::Import {
            export: incomplete, package_root: recovery_root.to_string_lossy().into_owned(), enable: None,
        }).unwrap();
        let pending = recovering.installed_unit_version("middle", "1.0.0", &a.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(pending.edges[0].status, EdgeStatus::Unresolved);
        assert_eq!(pending.edges[0].selected_artifact_digest, Some(one.unit.artifact_digest.clone()));
        assert_eq!(recovering.unit_status("top").unwrap()["closure"]["state"], json!("incomplete"));
        import(&mut recovering, &recovery_root, vec![one]);
        assert_eq!(recovering.unit_status("top").unwrap()["closure"]["state"], json!("complete"));
    }

    #[test]
    fn schema_two_transfer_does_not_invent_an_old_selection() {
        let mut node = Node::open(&scratch("old-wire-node")).unwrap();
        let root = scratch("old-wire-packages");
        let dependency = package("target", "1.0.0", b"wire-target", vec![]);
        let source = package("source", "1.0.0", b"wire-source", vec![exact("edge", "target", None)]);
        let mut old_transfer = export(vec![source.clone(), dependency.clone()]);
        old_transfer.schema = 2;
        old_transfer.root_revision = None;
        old_transfer.root_artifact_digest = None;
        let imported = node.handle_unit_request(UnitRequest::Import {
            export: old_transfer, package_root: root.to_string_lossy().into_owned(), enable: None,
        }).unwrap();
        assert_eq!(imported["dependency_resolutions"][0]["offered_edges"][0]["status"], json!("legacy_ambiguous"));
        import(&mut node, &root, vec![dependency]);
        let transfer = node.export_unit_closure(&node.installed_unit("source").unwrap().unwrap()).unwrap();
        let mut receiver = Node::open(&scratch("old-wire-receiver")).unwrap();
        let receiver_root = scratch("old-wire-receiver-packages");
        receiver.handle_unit_request(UnitRequest::Import {
            export: transfer, package_root: receiver_root.to_string_lossy().into_owned(), enable: None,
        }).unwrap();
        assert_eq!(receiver.unit_status("source").unwrap()["closure"]["edges"][0]["status"], json!("legacy_ambiguous"));
    }

    #[test]
    fn package_transfers_between_nodes_without_inheriting_readiness() {
        let dir_a = scratch("node-a");
        let dir_b = scratch("node-b");
        let root_a = scratch("packages-a");
        let root_b = scratch("packages-b");
        let mut a = Node::open(&dir_a).unwrap();
        let dependency = package("demo.skill.base", "1.0.0", b"base-bytes", vec![]);
        let root = package(
            "demo.skill.root",
            "2.1.0",
            b"root-bytes",
            vec![DependencySpec {
                edge_id: "base".into(),
                strength: DependencyStrength::Required,
                selector: DependencySelector::Exact {
                    unit_id: "demo.skill.base".into(),
                    revision_requirement: Some(">=1.0.0 <2.0.0".into()),
                    artifact_digest: None,
                    contract_requirement: None,
                },
                purpose: None,
            }],
        );
        a.handle_unit_request(UnitRequest::Import {
            export: export(vec![root]),
            package_root: root_a.to_string_lossy().into_owned(),
            enable: None,
        })
        .unwrap();
        // The origin node proves the unit by running the materialized artifact.
        let proven = a
            .handle_unit_request(trial(
                "demo.skill.root",
                vec!["/bin/sh", "-c", "cat {artifact}"],
            ))
            .unwrap();
        assert_eq!(proven["outcome"], json!("succeeded"));
        assert_eq!(proven["artifact_bound"], json!(true));
        assert_eq!(proven["readiness_proof"], json!(true));
        assert_eq!(proven["readiness"], json!("unresolved"));
        a.handle_unit_request(UnitRequest::Import {
            export: export(vec![dependency]),
            package_root: root_a.to_string_lossy().into_owned(),
            enable: None,
        })
        .unwrap();
        let resolved = a
            .handle_unit_request(UnitRequest::Status {
                unit_id: "demo.skill.root".into(),
            })
            .unwrap();
        assert_eq!(resolved["closure"]["state"], json!("complete"));
        assert_eq!(resolved["readiness"]["state"], json!("ready"));
        let exported = a
            .handle_unit_request(UnitRequest::Export {
                unit_id: "demo.skill.root".into(),
            })
            .unwrap();
        let export: UnitExport = serde_json::from_value(exported["export"].clone()).unwrap();

        let mut b = Node::open(&dir_b).unwrap();
        b.handle_unit_request(UnitRequest::Import {
            export,
            package_root: root_b.to_string_lossy().into_owned(),
            enable: None,
        })
        .unwrap();
        let status = b
            .handle_unit_request(UnitRequest::Status {
                unit_id: "demo.skill.root".into(),
            })
            .unwrap();
        // The receiving node holds the same unit, but it is not ready here.
        assert_eq!(status["unit_id"], json!("demo.skill.root"));
        assert_eq!(status["revision"], json!("2.1.0"));
        assert_eq!(
            status["artifact_digest"],
            json!(Digest::blake3(b"root-bytes").0)
        );
        assert_eq!(status["config"], json!({"mode": "test"}));
        assert_eq!(status["readiness"]["state"], json!("unverified"));
        assert_eq!(status["local_trials"], json!(0));
        assert!(status["remote_evidence"].as_u64().unwrap() >= 1);
        assert_eq!(status["closure"]["state"], json!("complete"));
        assert_eq!(status["materialized"], json!(true));
        // The advisory label does not deny the trial.
        let notes = status["advisory"].as_array().unwrap();
        assert!(notes.iter().any(|n| n.as_str().unwrap().contains("plan9")));
        // An unbound investigation is allowed and recorded, but it is not a
        // readiness proof.
        let investigation = b
            .handle_unit_request(trial("demo.skill.root", vec!["/bin/sh", "-c", "exit 0"]))
            .unwrap();
        assert_eq!(investigation["outcome"], json!("succeeded"));
        assert_eq!(investigation["artifact_bound"], json!(false));
        assert_eq!(investigation["readiness_proof"], json!(false));
        assert_eq!(investigation["readiness"], json!("unverified"));
        // The local proof of the artifact makes this node ready.
        let proven = b
            .handle_unit_request(trial(
                "demo.skill.root",
                vec!["/bin/sh", "-c", "cat {artifact}"],
            ))
            .unwrap();
        assert_eq!(proven["outcome"], json!("succeeded"));
        assert_eq!(proven["readiness_proof"], json!(true));
        assert_eq!(proven["readiness"], json!("ready"));
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }
}
