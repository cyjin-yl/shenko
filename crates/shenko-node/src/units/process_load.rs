//! Read-only ProcessLoad admission over the existing installation owner.
//!
//! One SQLite snapshot binds current enablement, environment selection,
//! descriptors (including config), exact dependency decisions and all bytes.
//! Preparation creates no processes, files, evidence, readiness or registry.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;
use shenko_types::process_load::ProcessLoad;

use crate::Node;
use super::capabilities::{CapabilityContract, PackageDeclaration};
use super::codec::{MAX_ARTIFACT_BYTES, MAX_EDGES, MAX_PACKAGES, UnitDescriptor, UnitKind,
    external_artifact_of, validate_descriptor};
use super::environment::{EnvironmentGeneration, EnvironmentSelector, MAX_ENVIRONMENT_MEMBERS,
    UNIT_ENVIRONMENT_SCHEMA, environment_closure_digest, environment_generation_key,
    read_environment_revision, read_environment_selector};
use super::installation::{StoredVersion, read_unit_selection_revision};
use super::resolution::{DependencySelector, DependencySpec, DependencyStrength, EdgeStatus,
    ResolvedDependency, edge_matches_spec, legacy_ambiguous, selection_matches, selection_of};

/// A retained UnitPackage identity, not an executable digest or host path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ProcessUnitIdentity {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
}

/// Structurally admitted bytes only. The owning adapters must separately
/// decode their role descriptors and prepare real implementations before any
/// runtime publication. All byte buffers are immutable, shared snapshots.
#[derive(Debug)]
pub struct AdmittedProcessLoad {
    unit: ProcessUnitIdentity,
    source: Arc<Version>,
    manifest: ProcessLoad,
    closure: Vec<ProcessUnitIdentity>,
    objects: BTreeMap<Digest, Arc<[u8]>>,
}

impl AdmittedProcessLoad {
    pub fn unit(&self) -> &ProcessUnitIdentity { &self.unit }
    /// The exact retained descriptor captured with these bytes, including
    /// adapter settings and credential/account/resource references. Config
    /// cannot replace executable/exports or grant access to those resources.
    pub fn descriptor(&self) -> &UnitDescriptor { &self.source.descriptor }
    pub fn manifest(&self) -> &ProcessLoad { &self.manifest }
    /// All exact selected package identities, including this manifest unit.
    pub fn closure(&self) -> &[ProcessUnitIdentity] { &self.closure }
    /// Frozen generation declaration when pinned, otherwise the installed
    /// descriptor's declaration. Requirements are never execution grants.
    pub fn declarations(&self) -> &PackageDeclaration { &self.source.declarations }
    /// Includes manifest, executable, files, role/effect/help documents and
    /// the remaining selected dependency artifacts. No global CAS lookup.
    pub fn objects(&self) -> &BTreeMap<Digest, Arc<[u8]>> { &self.objects }
    pub fn object_bytes(&self, digest: &Digest) -> Option<&[u8]> {
        self.objects.get(digest).map(AsRef::as_ref)
    }
}

/// An enabled, selected Code artifact is only a candidate. Ordinary WASM,
/// executable/data dependencies and malformed/unsupported manifests report
/// unavailable here; their installation/trial state is never rewritten.
#[derive(Debug)]
pub enum ProcessLoadAdmission {
    Admitted(AdmittedProcessLoad),
    Unavailable { unit: ProcessUnitIdentity, reason: String },
}
impl ProcessLoadAdmission {
    pub fn unit(&self) -> &ProcessUnitIdentity {
        match self { Self::Admitted(load) => load.unit(), Self::Unavailable { unit, .. } => unit }
    }
}

/// Opaque preparation receipt. Callers cannot manufacture or edit the seal,
/// admissions or byte buffers. This is a runtime read result, not another
/// durable installation record and not a health/publication receipt.
#[derive(Debug)]
pub struct ProcessLoadSnapshot {
    binding: Digest,
    environment: Option<EnvironmentSelector>,
    admissions: Vec<ProcessLoadAdmission>,
}
impl ProcessLoadSnapshot {
    pub fn binding(&self) -> &Digest { &self.binding }
    pub fn environment(&self) -> Option<&EnvironmentSelector> { self.environment.as_ref() }
    pub fn admissions(&self) -> &[ProcessLoadAdmission] { &self.admissions }
}

#[derive(Serialize)]
struct Selected {
    unit: ProcessUnitIdentity,
    /// Current can differ from the active environment pin after an import.
    current: Option<(String, Digest)>,
    enabled: bool,
    selection_revision: u64,
}
#[derive(Serialize)]
struct Selection {
    installation: EntityId,
    node: Option<EntityId>,
    environment_revision: u64,
    environment: Option<(EnvironmentSelector, EnvironmentGeneration)>,
    selected: Vec<Selected>,
}

fn identity(unit_id: String, revision: String, artifact_digest: Digest) -> ProcessUnitIdentity {
    ProcessUnitIdentity { unit_id, revision, artifact_digest }
}
fn enabled(value: i64) -> Result<bool> {
    ensure!(matches!(value, 0 | 1), "PROCESS_LOAD_SELECTION: invalid enable projection");
    Ok(value == 1)
}

impl Selection {
    fn read(conn: &rusqlite::Transaction<'_>) -> Result<Self> {
        let installation = serde_json::from_str(&crate::meta(conn, "installation")?
            .context("PROCESS_LOAD_SELECTION: installation identity is missing")?)?;
        let node = crate::meta(conn, "node")?.map(|data| serde_json::from_str(&data)).transpose()?;
        let environment_revision = read_environment_revision(conn)?;
        let environment = read_environment_selector(conn)?.map(|selector| -> Result<_> {
            let data = crate::meta(conn, &environment_generation_key(selector.generation))?
                .context("PROCESS_LOAD_SELECTION: selected environment is missing")?;
            let generation: EnvironmentGeneration = serde_json::from_str(&data)?;
            ensure!(selector.schema == UNIT_ENVIRONMENT_SCHEMA && generation.schema == UNIT_ENVIRONMENT_SCHEMA
                && selector.revision == environment_revision && selector.generation == generation.generation
                && selector.generation_digest == generation.digest
                && environment_closure_digest(&generation.roots, &generation.members, generation.closure_version)? == generation.digest,
                "PROCESS_LOAD_SELECTION: environment identity/digest differs");
            ensure!(generation.members.len() <= MAX_ENVIRONMENT_MEMBERS,
                "PROCESS_LOAD_SELECTION: environment member bound exceeded");
            let mut ids = BTreeSet::new();
            for member in &generation.members {
                ensure!(ids.insert(&member.unit_id), "PROCESS_LOAD_SELECTION: duplicate environment unit identity");
                ensure!(member.artifact_digest.canonical_blake3_hex().is_some(),
                    "PROCESS_LOAD_SELECTION: noncanonical environment artifact identity");
            }
            let mut roots = BTreeSet::new();
            for root in &generation.roots {
                ensure!(roots.insert(root) && ids.contains(root), "PROCESS_LOAD_SELECTION: missing or duplicate environment root");
            }
            Ok((selector, generation))
        }).transpose()?;
        let mut selected = Vec::new();
        if let Some((_, generation)) = &environment {
            for member in &generation.members {
                let row: Option<(String, String, i64)> = conn.query_row(
                    "SELECT revision,artifact_digest,enabled FROM unit_current WHERE unit_id=?1",
                    [&member.unit_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
                let (current, is_enabled) = match row {
                    Some((revision, digest, state)) => (Some((revision, Digest(digest))), enabled(state)?),
                    None => (None, false),
                };
                selected.push(Selected { unit: identity(member.unit_id.clone(), member.revision.clone(), member.artifact_digest.clone()),
                    current, enabled: is_enabled, selection_revision: read_unit_selection_revision(conn, &member.unit_id)? });
            }
        } else {
            // Stream only enabled Code roots, including unreadable/unknown
            // descriptor shapes so corruption cannot hide a candidate. Pure
            // content and disabled rows consume no capture quota or seal.
            // Repeat this discovery on revalidation: no cached root list or
            // installation-wide limit can hide a newly enabled Code unit.
            let mut statement = conn.prepare(
                "SELECT c.unit_id,c.revision,c.artifact_digest,c.enabled FROM unit_current AS c \
                 LEFT JOIN unit_versions AS v ON v.unit_id=c.unit_id AND v.revision=c.revision AND v.artifact_digest=c.artifact_digest \
                 WHERE c.enabled<>0 AND CASE WHEN json_valid(v.data) THEN \
                   COALESCE(json_extract(v.data,'$.unit.kind') NOT IN ('perception','awareness','skill','metapackage','distribution'),1) \
                   ELSE 1 END ORDER BY c.unit_id")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                ensure!(selected.len() < MAX_ENVIRONMENT_MEMBERS,
                    "PROCESS_LOAD_CLOSURE: captured version bound exceeded by enabled Code roots");
                let (unit_id, revision, digest, state): (String, String, String, i64) =
                    (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
                let selection_revision = read_unit_selection_revision(conn, &unit_id)?;
                let digest = Digest(digest);
                ensure!(digest.canonical_blake3_hex().is_some(), "PROCESS_LOAD_SELECTION: noncanonical selected artifact identity");
                selected.push(Selected { current: Some((revision.clone(), digest.clone())),
                    unit: identity(unit_id, revision, digest), enabled: enabled(state)?, selection_revision });
            }
        }
        selected.sort_by(|left, right| left.unit.cmp(&right.unit));
        Ok(Self { installation, node, environment_revision, environment, selected })
    }
}

#[derive(Serialize)]
struct VersionSeal {
    /// Full StoredVersion bytes, including config and imported_at. Absence
    /// is sealed too, so repairing an unavailable exact version is stale.
    descriptor: Option<Digest>,
    edges: Option<Digest>,
    selection_revision: u64,
}
#[derive(Debug)]
struct Version {
    descriptor: UnitDescriptor,
    edges: Vec<ResolvedDependency>,
    declarations: PackageDeclaration,
}
struct Source<'a> {
    conn: &'a Connection,
    selection: &'a Selection,
    versions: BTreeMap<ProcessUnitIdentity, Arc<Version>>,
    seals: BTreeMap<ProcessUnitIdentity, VersionSeal>,
    objects: BTreeMap<Digest, Arc<[u8]>>,
}

#[derive(Debug)]
struct CaptureLimit;
impl std::fmt::Display for CaptureLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PROCESS_LOAD_CLOSURE: captured version bound exceeded")
    }
}
impl std::error::Error for CaptureLimit {}

impl<'a> Source<'a> {
    fn new(conn: &'a Connection, selection: &'a Selection) -> Self {
        Self { conn, selection, versions: BTreeMap::new(), seals: BTreeMap::new(), objects: BTreeMap::new() }
    }
    fn version(&mut self, unit: &ProcessUnitIdentity) -> Result<Arc<Version>> {
        if let Some(version) = self.versions.get(unit) { return Ok(Arc::clone(version)); }
        if !self.seals.contains_key(unit) && self.seals.len() >= MAX_ENVIRONMENT_MEMBERS {
            return Err(CaptureLimit.into());
        }
        let data: Option<String> = self.conn.query_row(
            "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit.unit_id, unit.revision, unit.artifact_digest.0], |row| row.get(0)).optional()?;
        // Seal before parsing any candidate metadata. Invalid declarations
        // are unavailable locally, but their correction must invalidate the
        // snapshot even if the error text and artifact digest stay equal.
        self.seals.insert(unit.clone(), VersionSeal {
            descriptor: data.as_ref().map(|data| Digest::blake3(data.as_bytes())), edges: None,
            selection_revision: read_unit_selection_revision(self.conn, &unit.unit_id)? });
        let data = data.context("PROCESS_LOAD_CLOSURE: exact selected version is not retained")?;
        let stored: StoredVersion = serde_json::from_str(&data)?;
        validate_descriptor(&stored.unit)?;
        ensure!(stored.unit.unit_id == unit.unit_id && stored.unit.revision == unit.revision
            && stored.unit.artifact_digest == unit.artifact_digest,
            "PROCESS_LOAD_CLOSURE: selected descriptor identity differs");
        let (edges, declarations) = if let Some((_, generation)) = &self.selection.environment {
            let member = generation.members.iter().find(|member| member.unit_id == unit.unit_id
                && member.revision == unit.revision && member.artifact_digest == unit.artifact_digest)
                .context("PROCESS_LOAD_CLOSURE: dependency is not the active environment's exact pin")?;
            (member.edges.clone(), member.declarations.clone())
        } else {
            // Read only the bounded declared edge slots. Historical rows
            // from removed declarations do not consume this closure's quota.
            let rows = stored.unit.dependencies.iter().map(|spec| -> Result<_> {
                let data: Option<String> = self.conn.query_row(
                    "SELECT data FROM unit_edges WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 AND edge_id=?4",
                    params![unit.unit_id, unit.revision, unit.artifact_digest.0, spec.edge_id], |row| row.get(0)).optional()?;
                Ok((&spec.edge_id, data))
            }).collect::<Result<Vec<_>>>()?;
            self.seals.get_mut(unit).expect("captured version seal").edges =
                Some(Digest::blake3(&serde_json::to_vec(&rows)?));
            let mut edges = stored.unit.dependencies.iter().zip(rows).map(|(spec, (id, data))| -> Result<_> {
                let edge: Option<ResolvedDependency> = data.as_deref().map(serde_json::from_str).transpose()?;
                ensure!(edge.as_ref().is_none_or(|edge| id == &edge.edge_id),
                    "PROCESS_LOAD_CLOSURE: dependency row identity differs");
                Ok(edge.filter(|edge| edge_matches_spec(edge, spec)).unwrap_or_else(|| {
                    legacy_ambiguous(spec, "no matching digest-bound dependency selection is retained")
                }))
            }).collect::<Result<Vec<_>>>()?;
            edges.sort_by(|left, right| left.edge_id.cmp(&right.edge_id));
            (edges, stored.unit.package_declaration()?)
        };
        if self.selection.environment.is_some() {
            self.seals.get_mut(unit).expect("captured version seal").edges =
                Some(Digest::blake3(&serde_json::to_vec(&edges)?));
        }
        ensure!(edges.len() <= MAX_EDGES, "PROCESS_LOAD_CLOSURE: dependency count out of bounds");
        let mut ids = BTreeSet::new();
        for edge in &edges {
            ensure!(!edge.edge_id.is_empty() && edge.edge_id.len() <= 128 && ids.insert(&edge.edge_id),
                "PROCESS_LOAD_CLOSURE: missing or duplicate dependency identity");
            let selected = match (&edge.selected_unit_id, &edge.selected_revision, &edge.selected_artifact_digest) {
                (None, None, None) => false,
                (Some(id), Some(revision), Some(digest)) => {
                    ensure!(!id.is_empty() && !revision.is_empty() && digest.canonical_blake3_hex().is_some(),
                        "PROCESS_LOAD_CLOSURE: invalid exact dependency identity");
                    true
                }
                _ => anyhow::bail!("PROCESS_LOAD_CLOSURE: dependency has an incomplete selected identity"),
            };
            ensure!(edge.status != EdgeStatus::Satisfied || selected,
                "PROCESS_LOAD_CLOSURE: satisfied dependency has no selected identity");
            ensure!(edge.status != EdgeStatus::Omitted || (!selected && edge.strength == DependencyStrength::Recommended),
                "PROCESS_LOAD_CLOSURE: omitted dependency has an invalid selected identity/strength");
        }
        if let Some(capabilities) = declarations.capabilities() { capabilities.validate()?; }
        let version = Arc::new(Version { descriptor: stored.unit, edges, declarations });
        self.versions.insert(unit.clone(), Arc::clone(&version));
        Ok(version)
    }

    fn bytes(&mut self, unit: &ProcessUnitIdentity) -> Result<Arc<[u8]>> {
        let version = self.version(unit)?;
        ensure!(external_artifact_of(&version.descriptor).is_none(),
            "PROCESS_LOAD_CLOSURE: external artifact bytes are not transferred by UnitPackage");
        if let Some(bytes) = self.objects.get(&unit.artifact_digest) { return Ok(Arc::clone(bytes)); }
        let size: Option<u64> = self.conn.query_row("SELECT length(bytes) FROM objects WHERE digest=?1",
            [&unit.artifact_digest.0], |row| row.get(0)).optional()?;
        ensure!(size.is_some_and(|size| size <= MAX_ARTIFACT_BYTES as u64),
            "PROCESS_LOAD_CLOSURE: inline artifact is missing or exceeds the package byte bound");
        let bytes: Vec<u8> = self.conn.query_row("SELECT bytes FROM objects WHERE digest=?1",
            [&unit.artifact_digest.0], |row| row.get(0))?;
        ensure!(Some(bytes.len() as u64) == size && Digest::blake3(&bytes) == unit.artifact_digest,
            "PROCESS_LOAD_CLOSURE: selected artifact bytes are corrupt");
        let bytes: Arc<[u8]> = bytes.into();
        self.objects.insert(unit.artifact_digest.clone(), Arc::clone(&bytes));
        Ok(bytes)
    }

    fn admit(&mut self, unit: &ProcessUnitIdentity) -> Result<AdmittedProcessLoad> {
        let root = self.version(unit)?;
        ensure!(root.descriptor.kind == UnitKind::Code, "PROCESS_LOAD_KIND: selected unit is not Code");
        let manifest_bytes = self.bytes(unit)?;
        let manifest = ProcessLoad::decode(&manifest_bytes)?;
        manifest.validate_native_host()?;
        ensure!(unit.artifact_digest != manifest.executable.object.digest,
            "PROCESS_LOAD_CLOSURE: manifest and executable identities must be distinct");
        let mut pending = VecDeque::from([unit.clone()]);
        let mut closure = BTreeSet::new();
        let mut objects = BTreeMap::new();
        while let Some(member) = pending.pop_front() {
            if closure.contains(&member) { continue; }
            ensure!(closure.len() < MAX_PACKAGES, "PROCESS_LOAD_CLOSURE: closure exceeds UnitExport package bound");
            let version = self.version(&member)?;
            objects.insert(member.artifact_digest.clone(), self.bytes(&member)?);
            closure.insert(member);
            for edge in &version.edges {
                let selected = selection_of(Some(edge));
                let Some((unit_id, revision, digest)) = selected else {
                    ensure!(edge.strength == DependencyStrength::Recommended
                        && matches!(edge.status, EdgeStatus::Omitted | EdgeStatus::Unresolved),
                        "PROCESS_LOAD_CLOSURE: required/ambiguous dependency has no exact satisfied selection");
                    continue;
                };
                let target = identity(unit_id, revision, digest);
                let dependency = self.version(&target)?;
                validate_target(edge, &dependency.descriptor)?;
                pending.push_back(target);
            }
        }
        for reference in manifest.objects() {
            let bytes = objects.get(&reference.digest)
                .context("PROCESS_LOAD_CLOSURE: referenced object is not an artifact of this exact selected package closure")?;
            ensure!(bytes.len() as u64 == reference.bytes,
                "PROCESS_LOAD_CLOSURE: referenced object length differs from selected bytes");
        }
        Ok(AdmittedProcessLoad { unit: unit.clone(), manifest, closure: closure.into_iter().collect(),
            source: root, objects })
    }
}

fn validate_target(edge: &ResolvedDependency, target: &UnitDescriptor) -> Result<()> {
    let spec = DependencySpec { edge_id: edge.edge_id.clone(), strength: edge.strength,
        selector: edge.selector.clone(), purpose: None };
    ensure!(selection_matches(&spec, &target.unit_id, &target.revision, &target.artifact_digest, &target.provides),
        "PROCESS_LOAD_CLOSURE: pinned dependency no longer satisfies its selector");
    if let Some(contract) = &edge.selected_contract {
        ensure!(target.provides_contract(contract), "PROCESS_LOAD_CLOSURE: selected contract is not provided by the exact dependency");
    }
    match &edge.selector {
        DependencySelector::Exact { revision_requirement, artifact_digest, contract_requirement, .. } => {
            ensure!(revision_requirement.is_some() || artifact_digest.is_some(),
                "PROCESS_LOAD_CLOSURE: exact dependency has no version/artifact pin");
            if let Some(contract) = contract_requirement {
                ensure!(target.provides_contract(contract) && edge.selected_contract.as_ref() == Some(contract),
                    "PROCESS_LOAD_CLOSURE: pinned service contract differs");
            }
        }
        DependencySelector::Abstract { capability_ref, .. } if target.kind == UnitKind::Code => {
            let contract = CapabilityContract::parse(capability_ref.as_deref()
                .context("PROCESS_LOAD_CLOSURE: Code dependency cannot be selected by topic alone")?)?;
            ensure!(target.provides_contract(&contract) && edge.selected_contract.as_ref() == Some(&contract),
                "PROCESS_LOAD_CLOSURE: Code dependency has no exact selected capability contract");
        }
        DependencySelector::Abstract { .. } => {},
    }
    Ok(())
}

fn capture(conn: &rusqlite::Transaction<'_>) -> Result<ProcessLoadSnapshot> {
    let selection = Selection::read(conn)?;
    let mut source = Source::new(conn, &selection);
    let mut admissions = Vec::new();
    for selected in selection.selected.iter().filter(|selected| selected.enabled) {
        let candidate = (|| -> Result<_> {
            let version = source.version(&selected.unit)?;
            if version.descriptor.kind != UnitKind::Code { return Ok(None); }
            source.admit(&selected.unit).map(Some)
        })();
        match candidate {
            Ok(Some(load)) => admissions.push(ProcessLoadAdmission::Admitted(load)),
            Ok(None) => {},
            // Database failures and whole-capture capacity are not a
            // plugin's unsupported-load status. Neither can be hidden as an
            // unavailable candidate with an incomplete source seal.
            Err(error) if error.is::<rusqlite::Error>() || error.is::<CaptureLimit>() => return Err(error),
            Err(error) => admissions.push(ProcessLoadAdmission::Unavailable {
                unit: selected.unit.clone(), reason: format!("{error:#}") }),
        }
    }
    let outcomes: Vec<_> = admissions.iter().map(|admission| (admission.unit(), match admission {
        ProcessLoadAdmission::Admitted(_) => None,
        ProcessLoadAdmission::Unavailable { reason, .. } => Some(reason),
    })).collect();
    // Structured identity keys are serialized as sorted entries, not JSON
    // object keys. A config-only reimport, ABA enablement, changed frozen
    // edge, changed selector, missing/corrupt bytes or new candidate changes
    // this receipt even if a manifest digest remains the same.
    let versions: Vec<_> = source.seals.iter().collect();
    let binding = Digest::blake3(&serde_json::to_vec(&( "node.process-load-selection/v1", &selection, versions, outcomes ))?);
    Ok(ProcessLoadSnapshot { binding, environment: selection.environment.as_ref().map(|(selector, _)| selector.clone()), admissions })
}

impl Node {
    /// Prepare the enabled Code candidates and their actual byte closures in
    /// one existing Node read transaction. Environment pins/locked edges win
    /// whenever an environment is active; otherwise current roots and exact
    /// installed dependency decisions win. Data dependencies need not be
    /// independently enabled: reading their bytes never launches their code.
    /// Inline artifacts only; external locators are never dereferenced.
    pub fn process_load_snapshot(&mut self) -> Result<ProcessLoadSnapshot> {
        ensure!(!self.failed, "Node is in failed state");
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let snapshot = capture(&tx)?;
        tx.commit()?;
        Ok(snapshot)
    }

    /// Re-read every binding and byte under the physical installation permit
    /// and the existing Node writer
    /// exclusion, then perform one bounded synchronous in-memory generation
    /// swap. Stale selection refuses before calling `publish`.
    ///
    /// Prepare processes, owning adapters and all fallible runtime resources
    /// BEFORE this method. Lock order: physical installation permit, Node
    /// write transaction, then only the runtime generation-swap lock. Never hold a runtime lock while entering
    /// Node. `publish` must do no process/factory work, filesystem/Store/Node
    /// access, other I/O, awaiting or old-generation retirement; it must not
    /// reenter Node. Return the displaced generation and its retained
    /// resources as part of `T`, and dispose of them after this method returns;
    /// dropping large old byte/descriptor maps inside `publish` is forbidden.
    /// Bytes retained by old owners/receipts are unaffected.
    ///
    /// This is a publication fence, not publication itself: it writes no
    /// installation, health, trial, operation or capability record.
    pub fn with_revalidated_process_loads<T>(&mut self, expected: &ProcessLoadSnapshot,
        publish: impl FnOnce(&ProcessLoadSnapshot) -> Result<T>,
    ) -> Result<T> {
        ensure!(!self.failed, "Node is in failed state");
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        let current = capture(&tx)?;
        ensure!(current.binding == expected.binding,
            "PROCESS_LOAD_SELECTION_STALE: selected source/config/environment/dependency binding changed");
        let result = publish(&current)?;
        tx.commit()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;

    use super::*;
    use crate::units::{EnvironmentRoot, UnitExport, UnitPackage, UnitRequest, b64_decode, b64_encode};
    use crate::units::test_support::{exact, export, package, scratch};
    use serde_json::json;
    use shenko_types::object::{ArtifactRef, ObjectRef, SchemaRef, VersionedRef};
    use shenko_types::process_load::{PROCESS_LOAD_MAJOR, PROCESS_LOAD_SCHEMA, PROCESS_OBJECT_NAMESPACE,
        ProcessExport, ProcessFile, ProcessPlatform, ProcessProtocol, ProcessRole};

    fn reference(package: &UnitPackage) -> ObjectRef {
        let digest = package.unit.artifact_digest.clone();
        ObjectRef { locator: format!("sqlite:objects/{digest}"), digest,
            bytes: b64_decode(&package.artifact_base64).unwrap().len() as u64,
            media_type: "application/octet-stream".into(), store_namespace: PROCESS_OBJECT_NAMESPACE.into(),
            codec: SchemaRef { name: "raw".into(), major: 1, digest: Digest::blake3(b"raw") } }
    }
    fn fixture() -> (ProcessLoad, Vec<UnitPackage>) {
        // These are admission byte fixtures. No test claims they are a
        // runnable native program or installs a fake runtime/role handler.
        let mut executable = package("native-program", "1.0.0", b"program-artifact-one", vec![]);
        executable.unit.kind = UnitKind::Code;
        let file = package("lookup-data", "1.0.0", b"immutable lookup data", vec![]);
        let help = package("role-help", "1.0.0", b"This fixture only checks byte closure admission.", vec![]);
        let effect = package("role-effects", "1.0.0", br#"{"schema":"fixture.effect","major":1}"#, vec![]);
        let descriptors = [
            package("task-descriptor", "1.0.0", br#"{"schema":"fixture.task","major":1}"#, vec![]),
            package("provider-descriptor", "1.0.0", br#"{"schema":"fixture.provider","major":1}"#, vec![]),
            package("channel-descriptor", "1.0.0", br#"{"schema":"fixture.channel","major":1}"#, vec![]),
        ];
        let executable_ref = reference(&executable);
        let symbol = |name: &str| VersionedRef { artifact: ArtifactRef {
            digest: executable_ref.digest.clone(), locator: executable_ref.locator.clone() }, symbol: name.into(),
            schema: SchemaRef { name: "fixture.process".into(), major: 1, digest: Digest::blake3(b"fixture.process/v1") } };
        let mut exports = Vec::new();
        for ((role, name), descriptor) in [(ProcessRole::Task, "task"), (ProcessRole::Provider, "provider"),
            (ProcessRole::Channel, "channel")].into_iter().zip(&descriptors)
        {
            exports.push(ProcessExport { role, export_id: format!("fixture.{name}"), descriptor: reference(descriptor),
                implementation: symbol(name), effects: vec![reference(&effect)], help: reference(&help),
                receipt_reader: symbol("read_receipt"), reconcile: Some(symbol("reconcile")) });
        }
        let load = ProcessLoad { schema: PROCESS_LOAD_SCHEMA.into(), major: PROCESS_LOAD_MAJOR,
            protocol: ProcessProtocol::StdioJsonl, platform: ProcessPlatform::native_host(),
            executable: ProcessFile { path: "bin/plugin".into(), object: executable_ref },
            files: vec![ProcessFile { path: "share/lookup".into(), object: reference(&file) }], exports };
        let mut dependencies = vec![executable, file, help, effect];
        dependencies.extend(descriptors);
        (load, dependencies)
    }
    fn packages(load: &ProcessLoad, dependencies: &[UnitPackage]) -> Vec<UnitPackage> {
        let edges = dependencies.iter().map(|dependency| exact(&dependency.unit.unit_id,
            &dependency.unit.unit_id, Some(dependency.unit.artifact_digest.clone()))).collect();
        let mut root = package("process", "1.0.0", &serde_json::to_vec(load).unwrap(), edges);
        root.unit.kind = UnitKind::Code;
        let mut packages = vec![root]; packages.extend_from_slice(dependencies); packages
    }
    fn install(node: &mut Node, root: &Path, packages: Vec<UnitPackage>) {
        receive(node, root, export(packages));
    }
    fn receive(node: &mut Node, root: &Path, export: UnitExport) {
        node.handle_unit_request(UnitRequest::Import { export, package_root: root.to_string_lossy().into_owned(),
            enable: Some(true) }).unwrap();
    }
    fn admitted<'a>(snapshot: &'a ProcessLoadSnapshot, id: &str) -> &'a AdmittedProcessLoad {
        match snapshot.admissions().iter().find(|entry| entry.unit().unit_id == id).expect("selected candidate") {
            ProcessLoadAdmission::Admitted(load) => load,
            ProcessLoadAdmission::Unavailable { reason, .. } => panic!("unexpected unavailable candidate: {reason}"),
        }
    }
    fn unavailable<'a>(snapshot: &'a ProcessLoadSnapshot, id: &str) -> &'a str {
        match snapshot.admissions().iter().find(|entry| entry.unit().unit_id == id).expect("selected candidate") {
            ProcessLoadAdmission::Unavailable { reason, .. } => reason,
            ProcessLoadAdmission::Admitted(_) => panic!("unexpected admission"),
        }
    }
    fn transfer(node: &Node) -> UnitExport {
        node.export_installation_units().unwrap().into_iter().find(|export| export.root_unit_id == "process").unwrap()
    }
    fn activate_current(node: &mut Node) -> u64 {
        let prepared = node.handle_unit_request(UnitRequest::PrepareEnvironment { roots: vec![EnvironmentRoot {
            unit_id: "process".into(), revision: None, artifact_digest: None }] }).unwrap();
        let generation = prepared["generation"]["generation"].as_u64().unwrap();
        let revision = node.environment_selection_revision().unwrap();
        node.handle_unit_request(UnitRequest::ActivateEnvironment { generation, expected_revision: revision }).unwrap();
        generation
    }
    fn selection_stale(node: &mut Node, snapshot: &ProcessLoadSnapshot) {
        let called = Cell::new(false);
        let result = node.with_revalidated_process_loads(snapshot, |_| { called.set(true); Ok(()) });
        assert!(result.unwrap_err().to_string().contains("PROCESS_LOAD_SELECTION_STALE"));
        assert!(!called.get(), "a stale source must never reach generation publication");
    }

    #[test]
    fn process_load_install_export_import_preserves_exact_three_role_closure() {
        let dir = scratch("process-admission-origin");
        let root = scratch("process-admission-origin-packages");
        let mut origin = Node::open(&dir).unwrap();
        let (load, dependencies) = fixture();
        let installed = packages(&load, &dependencies);
        install(&mut origin, &root, installed.clone());
        let before = origin.installed_unit_fact("process").unwrap().unwrap();
        let snapshot = origin.process_load_snapshot().unwrap();
        let selected = admitted(&snapshot, "process");
        assert_eq!(selected.manifest(), &load);
        assert_eq!(snapshot.admissions().len(), 1, "disabled Code byte dependencies are never independent processes");
        assert_ne!(selected.unit().artifact_digest, selected.manifest().executable.object.digest);
        assert_eq!(selected.closure().len(), installed.len());
        for package in &installed {
            assert_eq!(selected.object_bytes(&package.unit.artifact_digest).unwrap(), b64_decode(&package.artifact_base64).unwrap());
        }
        let outgoing = transfer(&origin);
        assert_eq!(outgoing.packages.len(), installed.len());
        assert!(outgoing.packages.iter().all(|package| package.resolved_dependencies.is_some()));
        let mut receiver = Node::open(&scratch("process-admission-receiver")).unwrap();
        receive(&mut receiver, &scratch("process-admission-receiver-packages"), outgoing);
        let received = receiver.process_load_snapshot().unwrap();
        assert_eq!(admitted(&received, "process").manifest(), &load);
        for package in &installed {
            assert_eq!(admitted(&received, "process").object_bytes(&package.unit.artifact_digest).unwrap(),
                selected.object_bytes(&package.unit.artifact_digest).unwrap());
        }
        assert_eq!(origin.conn.query_row("SELECT count(*) FROM unit_trials", [], |row| row.get::<_, u64>(0)).unwrap(), 0);
        assert_eq!(origin.installed_unit_fact("process").unwrap().unwrap().descriptor, before.descriptor);
        drop(origin);
        let mut reopened = Node::open(&dir).unwrap();
        assert_eq!(reopened.process_load_snapshot().unwrap().binding(), snapshot.binding());
        let copied_digest = reopened.with_revalidated_process_loads(&snapshot,
            |current| Ok(admitted(current, "process").manifest().executable.object.digest.clone())).unwrap();
        assert_eq!(copied_digest, load.executable.object.digest);
    }

    #[test]
    fn process_load_malformed_and_unsupported_manifests_stay_installed_without_roles() {
        let (load, dependencies) = fixture();
        let text = serde_json::to_string(&load).unwrap();
        let mut unsupported_major = load.clone(); unsupported_major.major += 1;
        let mut foreign = load.clone(); foreign.platform.arch = "unsupported-arch".into();
        let mut duplicate_role = load.clone(); duplicate_role.exports.push(load.exports[0].clone());
        let cases = [
            b"ordinary Code data, not a ProcessLoad manifest".to_vec(),
            serde_json::to_vec(&unsupported_major).unwrap(),
            serde_json::to_vec(&foreign).unwrap(),
            serde_json::to_vec(&duplicate_role).unwrap(),
            text.replacen("\"major\":1", "\"major\":1,\"major\":1", 1).into_bytes(),
            text.replacen("\"codec\":{", "\"codec\":{\"unknown\":0,", 1).into_bytes(),
            text.replacen("\"protocol\":\"stdio_jsonl\"", "\"protocol\":\"rust_abi\"", 1).into_bytes(),
            format!("{text} {{}}").into_bytes(),
        ];
        for (index, bytes) in cases.into_iter().enumerate() {
            let mut node = Node::open(&scratch(&format!("process-admission-invalid-{index}"))).unwrap();
            let mut incoming = packages(&load, &dependencies);
            incoming[0].artifact_base64 = b64_encode(&bytes);
            incoming[0].unit.artifact_digest = Digest::blake3(&bytes);
            let digest = incoming[0].unit.artifact_digest.clone();
            install(&mut node, &scratch(&format!("process-admission-invalid-packages-{index}")), incoming);
            let snapshot = node.process_load_snapshot().unwrap();
            assert!(!unavailable(&snapshot, "process").is_empty());
            assert_eq!(node.unit_content("process", "1.0.0", Some(&digest)).unwrap().bytes, bytes);
            assert_eq!(node.conn.query_row("SELECT count(*) FROM unit_trials", [], |row| row.get::<_, u64>(0)).unwrap(), 0);
        }
    }

    #[test]
    fn process_load_config_only_reimport_cannot_redirect_load_and_invalidates_preparation() {
        let mut node = Node::open(&scratch("process-admission-config")).unwrap();
        let root = scratch("process-admission-config-packages");
        let (load, dependencies) = fixture();
        let mut incoming = packages(&load, &dependencies);
        incoming[0].unit.config = json!({ "adapter": { "account_ref": "fixture-account-a",
            "credential_ref": "fixture-credential-a", "resource_ref": "fixture-resource-a" } });
        let original_descriptor = incoming[0].unit.clone();
        install(&mut node, &root, incoming.clone());
        let before = node.process_load_snapshot().unwrap();
        assert_eq!(admitted(&before, "process").descriptor(), &original_descriptor);
        incoming[0].unit.config = json!({ "adapter": { "account_ref": "fixture-account-b",
            "credential_ref": "fixture-credential-b", "resource_ref": "fixture-resource-b" },
            "program": "/host/another-program", "process_load": { "major": 9 },
            "roles": ["invented-provider"] });
        let replacement_descriptor = incoming[0].unit.clone();
        install(&mut node, &root, incoming);
        selection_stale(&mut node, &before);
        let after = node.process_load_snapshot().unwrap();
        assert_eq!(admitted(&before, "process").descriptor(), &original_descriptor,
            "an old preparation owns its exact immutable descriptor after reimport");
        assert_eq!(admitted(&after, "process").descriptor(), &replacement_descriptor);
        assert_eq!(admitted(&after, "process").manifest(), &load);
        assert_eq!(admitted(&after, "process").unit().artifact_digest, admitted(&before, "process").unit().artifact_digest);
        assert_ne!(after.binding(), before.binding());
    }

    #[test]
    fn process_load_requires_selected_bytes_instead_of_global_object_presence() {
        let mut node = Node::open(&scratch("process-admission-not-global-cas")).unwrap();
        let root = scratch("process-admission-not-global-cas-packages");
        let (load, dependencies) = fixture();
        for dependency in &dependencies { install(&mut node, &root, vec![dependency.clone()]); }
        let mut incoming = packages(&load, &dependencies);
        incoming.truncate(1); incoming[0].unit.dependencies.clear();
        install(&mut node, &root, incoming);
        let snapshot = node.process_load_snapshot().unwrap();
        assert!(unavailable(&snapshot, "process").contains("exact selected package closure"));
        // Enabling ordinary code data exposes only an unavailable candidate,
        // despite that artifact being present and of kind Code.
        assert!(!unavailable(&snapshot, "native-program").is_empty());
    }

    #[test]
    fn process_load_exact_noncurrent_binary_pin_survives_transfer() {
        let mut node = Node::open(&scratch("process-admission-exact-pin")).unwrap();
        let root = scratch("process-admission-exact-pin-packages");
        let (load, dependencies) = fixture();
        install(&mut node, &root, packages(&load, &dependencies));
        let before = node.process_load_snapshot().unwrap();
        let mut replacement = package("native-program", "1.0.0", b"program-artifact-two", vec![]);
        replacement.unit.kind = UnitKind::Code;
        install(&mut node, &root, vec![replacement.clone()]);
        selection_stale(&mut node, &before);
        let snapshot = node.process_load_snapshot().unwrap();
        let selected = admitted(&snapshot, "process");
        assert_eq!(selected.manifest().executable.object.digest, load.executable.object.digest);
        assert!(selected.object_bytes(&replacement.unit.artifact_digest).is_none());
        // Reimporting a retained noncurrent dependency changes its config
        // without changing unit_current or its selection revision. The byte
        // artifact is unchanged, so only the exact descriptor binding can
        // detect this preparation race.
        let revision = read_unit_selection_revision(&node.conn, "native-program").unwrap();
        let mut changed = dependencies[0].clone(); changed.unit.config = json!({ "setting": "changed" });
        install(&mut node, &root, vec![changed]);
        assert_eq!(read_unit_selection_revision(&node.conn, "native-program").unwrap(), revision);
        assert_eq!(node.installed_unit_fact("native-program").unwrap().unwrap().artifact_digest, replacement.unit.artifact_digest);
        selection_stale(&mut node, &snapshot);
        let refreshed = node.process_load_snapshot().unwrap();
        assert_eq!(admitted(&refreshed, "process").manifest(), &load);
        // A malformed noncurrent dependency is unavailable, not fatal to
        // candidate discovery. Its raw record remains in the seal before
        // package_declaration can fail, including repeated equal failures.
        let mut malformed = dependencies[0].clone();
        malformed.unit.config = json!({ "tool": { "capabilities": 42 }, "adapter": "first" });
        install(&mut node, &root, vec![malformed.clone()]);
        assert_eq!(read_unit_selection_revision(&node.conn, "native-program").unwrap(), revision);
        let failed = node.process_load_snapshot().unwrap();
        let reason = unavailable(&failed, "process").to_string();
        assert!(reason.contains("UNIT_PACKAGE_CAPABILITIES_INVALID"));
        malformed.unit.config["adapter"] = json!("second");
        install(&mut node, &root, vec![malformed]);
        let changed_failure = node.process_load_snapshot().unwrap();
        assert_eq!(unavailable(&changed_failure, "process"), reason.as_str());
        selection_stale(&mut node, &failed);
        let mut repaired = dependencies[0].clone(); repaired.unit.config = json!({ "setting": "repaired" });
        install(&mut node, &root, vec![repaired]);
        assert_eq!(read_unit_selection_revision(&node.conn, "native-program").unwrap(), revision);
        selection_stale(&mut node, &changed_failure);
        assert_eq!(admitted(&node.process_load_snapshot().unwrap(), "process").manifest(), &load);
        let outgoing = transfer(&node);
        assert!(outgoing.packages.iter().any(|package| package.unit.artifact_digest == load.executable.object.digest));
        assert!(!outgoing.packages.iter().any(|package| package.unit.artifact_digest == replacement.unit.artifact_digest));
        let mut receiver = Node::open(&scratch("process-admission-exact-pin-receiver")).unwrap();
        receive(&mut receiver, &scratch("process-admission-exact-pin-receiver-packages"), outgoing);
        let received = receiver.process_load_snapshot().unwrap();
        assert_eq!(admitted(&received, "process").manifest().executable.object.digest, load.executable.object.digest);
    }

    #[test]
    fn process_load_environment_pins_win_and_dropped_edges_cannot_be_resurrected() {
        let mut node = Node::open(&scratch("process-admission-environment")).unwrap();
        let root = scratch("process-admission-environment-packages");
        let (load, dependencies) = fixture();
        let mut first = packages(&load, &dependencies);
        first[0].unit.config = json!({ "adapter": { "account_ref": "pinned-account" },
            "tool": { "capabilities": { "declared": { "environment": ["PINNED_TOKEN"] } } } });
        let pinned_descriptor = first[0].unit.clone();
        install(&mut node, &root, first.clone());
        let generation_one = activate_current(&mut node);
        let pinned = node.process_load_snapshot().unwrap();
        let mut next_load = load.clone(); next_load.executable.path = "bin/next-plugin".into();
        let mut next = packages(&next_load, &dependencies); next[0].unit.revision = "2.0.0".into();
        next[0].unit.config = json!({ "adapter": { "account_ref": "current-account" },
            "tool": { "capabilities": { "declared": { "environment": ["CURRENT_TOKEN"] } } } });
        install(&mut node, &root, next.clone());
        let still_pinned = node.process_load_snapshot().unwrap();
        assert_eq!(still_pinned.environment().unwrap().generation, generation_one);
        assert_eq!(admitted(&still_pinned, "process").manifest(), &load);
        assert_eq!(admitted(&still_pinned, "process").descriptor(), &pinned_descriptor);
        assert_eq!(node.installed_unit_fact("process").unwrap().unwrap().descriptor, next[0].unit);
        selection_stale(&mut node, &pinned);
        // Reimporting the exact older pin may change adapter settings, but
        // the already frozen declaration remains the requirement authority.
        first[0].unit.config = json!({ "adapter": { "account_ref": "retained-account" },
            "tool": { "capabilities": { "declared": { "environment": ["RETAINED_TOKEN"] } } } });
        install(&mut node, &root, first.clone());
        selection_stale(&mut node, &still_pinned);
        let retained = node.process_load_snapshot().unwrap();
        assert_eq!(admitted(&retained, "process").descriptor(), &first[0].unit);
        assert_eq!(admitted(&retained, "process").declarations(), &pinned_descriptor.package_declaration().unwrap());
        assert_eq!(admitted(&still_pinned, "process").descriptor(), &pinned_descriptor);
        let generation_two = activate_current(&mut node);
        assert_ne!(generation_two, generation_one);
        selection_stale(&mut node, &still_pinned);
        let two = node.process_load_snapshot().unwrap();
        assert_eq!(admitted(&two, "process").manifest(), &next_load);
        // Reimport changes live descriptor/edge meaning at the same artifact
        // identity; the active environment's locked edges still own its
        // closure until a different generation is actually selected.
        next.truncate(1); next[0].unit.dependencies.clear();
        install(&mut node, &root, next);
        selection_stale(&mut node, &two);
        let frozen = node.process_load_snapshot().unwrap();
        assert_eq!(admitted(&frozen, "process").closure().len(), dependencies.len() + 1);
        let generation_three = activate_current(&mut node);
        assert_ne!(generation_three, generation_two);
        selection_stale(&mut node, &frozen);
        let dropped = node.process_load_snapshot().unwrap();
        assert!(unavailable(&dropped, "process").contains("exact selected package closure"));
    }

    #[test]
    fn process_load_resolver_contracts_survive_transfer_and_explicit_legacy_repair() {
        let mut node = Node::open(&scratch("process-admission-contract")).unwrap();
        let root = scratch("process-admission-contract-packages");
        let (load, mut dependencies) = fixture();
        let contract = CapabilityContract::parse("cap.fixture.process/1").unwrap();
        dependencies[0].unit.provides.push(contract.text());
        let mut incoming = packages(&load, &dependencies);
        let DependencySelector::Exact { contract_requirement, .. } = &mut incoming[0].unit.dependencies[0].selector else {
            panic!("fixture must use an exact executable dependency");
        };
        *contract_requirement = Some(contract.clone());
        incoming[0].unit.dependencies.push(DependencySpec { edge_id: "native-contract".into(),
            strength: DependencyStrength::Required, purpose: None,
            selector: DependencySelector::Abstract { capability_ref: Some(contract.text()), topic_ref: None } });
        let root_digest = incoming[0].unit.artifact_digest.clone();
        install(&mut node, &root, incoming);
        assert_eq!(admitted(&node.process_load_snapshot().unwrap(), "process").manifest(), &load);
        let outgoing = transfer(&node);
        let edges = outgoing.packages.iter().find(|package| package.unit.unit_id == "process").unwrap()
            .resolved_dependencies.as_ref().unwrap();
        let contracts: Vec<_> = edges.iter().filter(|edge| edge.selected_contract.is_some()).cloned().collect();
        assert_eq!(contracts.len(), 2, "both ordinary resolver paths must retain the requested contract");
        for edge in &contracts {
            assert_eq!(edge.status, EdgeStatus::Satisfied);
            assert_eq!(edge.selected_contract.as_ref(), Some(&contract));
            assert_eq!(edge.selected_artifact_digest.as_ref(), Some(&load.executable.object.digest));
        }
        let mut receiver = Node::open(&scratch("process-admission-contract-receiver")).unwrap();
        receive(&mut receiver, &scratch("process-admission-contract-receiver-packages"), outgoing);
        activate_current(&mut receiver);
        assert_eq!(admitted(&receiver.process_load_snapshot().unwrap(), "process").manifest(), &load);

        // Retain the old missing-contract footprint rather than teaching
        // admission or transfer to infer a contract that was never recorded.
        for mut edge in contracts {
            edge.selected_contract = None;
            node.conn.execute("UPDATE unit_edges SET data=?1 WHERE unit_id='process' AND revision='1.0.0' AND artifact_digest=?2 AND edge_id=?3",
                params![serde_json::to_string(&edge).unwrap(), root_digest.0, edge.edge_id]).unwrap();
        }
        let legacy_generation = activate_current(&mut node);
        let legacy_record = node.environment_generation(legacy_generation).unwrap().unwrap();
        // Import after activation so the inventory's preferred current
        // candidate actually differs from the environment's exact old pin.
        let mut replacement = package("native-program", "1.0.0", b"new compatible native artifact", vec![]);
        replacement.unit.kind = UnitKind::Code;
        replacement.unit.provides.push(contract.text());
        let replacement_digest = replacement.unit.artifact_digest.clone();
        install(&mut node, &root, vec![replacement]);
        let legacy_snapshot = node.process_load_snapshot().unwrap();
        assert!(unavailable(&legacy_snapshot, "process").contains("contract"));
        node.handle_unit_request(UnitRequest::ResolveVersion { unit_id: "process".into(), revision: "1.0.0".into(),
            artifact_digest: root_digest.clone() }).unwrap();
        let repaired = node.installed_unit_version("process", "1.0.0", &root_digest).unwrap().unwrap();
        for edge in repaired.edges.iter().filter(|edge| matches!(&edge.selector,
            DependencySelector::Exact { contract_requirement: Some(_), .. } | DependencySelector::Abstract { .. }))
        {
            assert_eq!(edge.selected_contract.as_ref(), Some(&contract));
            assert_eq!(edge.selected_artifact_digest.as_ref(), Some(&load.executable.object.digest));
        }
        assert_eq!(node.environment_generation(legacy_generation).unwrap().unwrap(), legacy_record);
        assert_eq!(node.installed_unit_fact("native-program").unwrap().unwrap().artifact_digest, replacement_digest);
        let still_legacy = node.process_load_snapshot().unwrap();
        assert_eq!(still_legacy.binding(), legacy_snapshot.binding(), "live repair must not rewrite frozen dependency meaning");
        assert!(unavailable(&still_legacy, "process").contains("contract"));
        assert_ne!(activate_current(&mut node), legacy_generation);
        assert_eq!(admitted(&node.process_load_snapshot().unwrap(), "process").manifest(), &load);
    }

    #[test]
    fn process_load_malformed_sibling_is_isolated_and_its_correction_is_stale() {
        let mut node = Node::open(&scratch("process-admission-malformed-sibling")).unwrap();
        let root = scratch("process-admission-malformed-sibling-packages");
        let (load, dependencies) = fixture();
        install(&mut node, &root, packages(&load, &dependencies));
        let mut sibling = packages(&load, &dependencies);
        sibling[0].unit.unit_id = "sibling".into();
        sibling[0].unit.config = json!({ "tool": { "capabilities": 42 } });
        install(&mut node, &root, sibling.clone());
        let malformed = node.process_load_snapshot().unwrap();
        assert_eq!(admitted(&malformed, "process").manifest(), &load);
        assert!(unavailable(&malformed, "sibling").contains("UNIT_PACKAGE_CAPABILITIES_INVALID"));
        sibling[0].unit.config = json!({ "adapter": { "account_ref": "sibling-account" },
            "tool": { "capabilities": { "declared": {} } } });
        install(&mut node, &root, sibling.clone());
        selection_stale(&mut node, &malformed);
        let corrected = node.process_load_snapshot().unwrap();
        assert_eq!(admitted(&corrected, "process").manifest(), &load);
        assert_eq!(admitted(&corrected, "sibling").descriptor(), &sibling[0].unit);
    }

    #[test]
    fn process_load_discovery_ignores_unrelated_inventory_and_rediscovers_code_roots() {
        let mut node = Node::open(&scratch("process-admission-large-inventory")).unwrap();
        let root = scratch("process-admission-large-inventory-packages");
        let unrelated: Vec<_> = (0..MAX_ENVIRONMENT_MEMBERS + 32).map(|index| {
            let mut unit = package(&format!("unrelated-{index:03}"), "1.0.0", b"unrelated content", vec![]);
            // Every chunk's root is content; Code members start disabled.
            if index % MAX_PACKAGES != 0 && index % 3 == 0 { unit.unit.kind = UnitKind::Code; }
            unit
        }).collect();
        for chunk in unrelated.chunks(MAX_PACKAGES) { install(&mut node, &root, chunk.to_vec()); }
        let (load, dependencies) = fixture();
        install(&mut node, &root, packages(&load, &dependencies));
        assert!(node.conn.query_row("SELECT count(*) FROM unit_current", [], |row| row.get::<_, u64>(0)).unwrap() > 256);
        let before = node.process_load_snapshot().unwrap();
        assert_eq!(before.admissions().len(), 1);
        assert_eq!(admitted(&before, "process").closure().len(), dependencies.len() + 1);
        let mut changed = unrelated[0].clone();
        changed.unit.config = json!({ "knowledge_setting": "changed" });
        install(&mut node, &root, vec![changed]);
        let after_content = node.process_load_snapshot().unwrap();
        assert_eq!(after_content.binding(), before.binding(), "unrelated content config is not a runtime source binding");
        node.with_revalidated_process_loads(&before, |_| Ok(())).unwrap();
        node.handle_unit_request(UnitRequest::SetEnabled { unit_id: "unrelated-003".into(), enabled: true }).unwrap();
        selection_stale(&mut node, &before);
        let enabled = node.process_load_snapshot().unwrap();
        assert!(!unavailable(&enabled, "unrelated-003").is_empty());
        assert_eq!(admitted(&enabled, "process").manifest(), &load);
        let mut changed_code = unrelated[3].clone(); changed_code.unit.config = json!({ "adapter_setting": "changed" });
        install(&mut node, &root, vec![changed_code]);
        selection_stale(&mut node, &enabled);
        let changed = node.process_load_snapshot().unwrap();
        let mut added = package("new-code-root", "1.0.0", b"new ordinary Code bytes", vec![]);
        added.unit.kind = UnitKind::Code;
        install(&mut node, &root, vec![added]);
        selection_stale(&mut node, &changed);
        assert!(!unavailable(&node.process_load_snapshot().unwrap(), "new-code-root").is_empty());
    }

    #[test]
    fn process_load_revalidation_rejects_enable_aba_and_another_installation() {
        let dir = scratch("process-admission-aba");
        let root = scratch("process-admission-aba-packages");
        let mut node = Node::open(&dir).unwrap();
        let (load, dependencies) = fixture();
        install(&mut node, &root, packages(&load, &dependencies));
        let snapshot = node.process_load_snapshot().unwrap();
        let mut editor = Node::open(&dir).unwrap();
        for enabled in [false, true] {
            editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: "process".into(), enabled }).unwrap();
        }
        selection_stale(&mut node, &snapshot);
        let current = node.process_load_snapshot().unwrap();
        let called = Cell::new(false);
        node.with_revalidated_process_loads(&current, |_| { called.set(true); Ok(()) }).unwrap();
        assert!(called.get());
        let mut foreign = Node::open(&scratch("process-admission-foreign-installation")).unwrap();
        install(&mut foreign, &scratch("process-admission-foreign-packages"), packages(&load, &dependencies));
        selection_stale(&mut foreign, &current);
    }

    #[test]
    fn process_load_rejects_wrong_lengths_corrupt_bytes_and_external_closure() {
        let (mut load, dependencies) = fixture();
        load.executable.object.bytes += 1;
        let mut wrong_length = Node::open(&scratch("process-admission-length")).unwrap();
        install(&mut wrong_length, &scratch("process-admission-length-packages"), packages(&load, &dependencies));
        assert!(unavailable(&wrong_length.process_load_snapshot().unwrap(), "process").contains("length differs"));

        let (load, dependencies) = fixture();
        let mut corrupt = Node::open(&scratch("process-admission-corrupt")).unwrap();
        install(&mut corrupt, &scratch("process-admission-corrupt-packages"), packages(&load, &dependencies));
        let before = corrupt.process_load_snapshot().unwrap();
        corrupt.conn.execute("UPDATE objects SET bytes=?1 WHERE digest=?2",
            params![b"altered bytes".as_slice(), load.executable.object.digest.0]).unwrap();
        selection_stale(&mut corrupt, &before);
        assert!(unavailable(&corrupt.process_load_snapshot().unwrap(), "process").contains("corrupt"));

        let (load, mut dependencies) = fixture();
        let executable = &mut dependencies[0];
        executable.unit.config = json!({ "external_artifact": { "digest": executable.unit.artifact_digest,
            "bytes": load.executable.object.bytes, "store": "cas", "locator": "/never/read/this/host/path" } });
        executable.artifact_base64.clear();
        let mut external = Node::open(&scratch("process-admission-external")).unwrap();
        install(&mut external, &scratch("process-admission-external-packages"), packages(&load, &dependencies));
        assert!(unavailable(&external.process_load_snapshot().unwrap(), "process").contains("external artifact"));
        let transferred = transfer(&external);
        assert!(transferred.packages.iter().find(|package| package.unit.unit_id == "native-program").unwrap().artifact_base64.is_empty());
    }
}
