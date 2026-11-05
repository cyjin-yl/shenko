use super::*;
use shenko_node::{UnitExport, UnitRequest};

pub(super) struct PreparedTrial {
    pub export: UnitExport,
    pub control_export: Option<UnitExport>,
    pub configuration_ref: ObjectRef,
}

#[derive(Serialize)]
struct RootIdentity<'a> {
    unit_id: &'a str,
    revision: &'a str,
    artifact_digest: &'a Digest,
}

#[derive(Serialize)]
struct FrozenConfiguration<'a> {
    schema_version: u32,
    source_export: &'a UnitExport,
    candidate_enable_root: bool,
    control_excluded_unit_id: &'a str,
    control_root: Option<RootIdentity<'a>>,
    control_enable_root: bool,
}

pub(super) fn prepare_trial(
    store: &Store,
    catalog: &CognitionCatalog,
    spec: &TrialSpec,
) -> Result<PreparedTrial> {
    let unit_id = spec.unit_id.trim();
    let revision = spec.revision.trim();
    ensure!(!unit_id.is_empty(), "the trial needs a unit id");
    ensure!(!revision.is_empty(), "the trial needs a revision");
    ensure!(!spec.scenarios.is_empty(), "a trial needs at least one scenario");
    ensure!(spec.scenarios.len() <= MAX_SCENARIOS,
        "a trial carries at most {MAX_SCENARIOS} scenarios");
    let export = catalog.export(unit_id)?;
    ensure!(export.root_unit_id == unit_id, "the exported root is not the requested unit");
    ensure!(export.root_revision.as_deref() == Some(revision),
        "the trial asked for revision '{revision}' but the installed current revision is '{}'; read the catalog again and trial the current version",
        export.root_revision.as_deref().unwrap_or("<missing>"));
    let artifact = export.root_artifact_digest.as_ref().context("the exported root has no artifact")?;
    ensure!(export.packages.iter().any(|package| package.unit.unit_id == unit_id
        && package.unit.revision == revision && &package.unit.artifact_digest == artifact),
        "the requested root version is absent from its export");

    // A disabled unit is still readable, and a learned candidate can still
    // have a discovery card. Exclude every version of this unit in control.
    // Keep other descriptors and pins unchanged; Node carries an absent pin
    // as unresolved rather than silently selecting a different dependency.
    let packages: Vec<_> = export.packages.iter()
        .filter(|package| package.unit.unit_id != unit_id).cloned().collect();
    let control_export = if let Some(root) = packages.first() {
        Some(UnitExport {
            schema: export.schema,
            root_unit_id: root.unit.unit_id.clone(),
            root_revision: Some(root.unit.revision.clone()),
            root_artifact_digest: Some(root.unit.artifact_digest.clone()),
            origin_node: export.origin_node.clone(),
            enabled: false,
            packages,
        })
    } else {
        None
    };
    let control_root = control_export.as_ref().and_then(|export| export.packages.first())
        .map(|root| RootIdentity { unit_id: &root.unit.unit_id,
            revision: &root.unit.revision, artifact_digest: &root.unit.artifact_digest });
    let configuration_ref = seal_trial_object(store, &FrozenConfiguration {
        schema_version: 1, source_export: &export, candidate_enable_root: true,
        control_excluded_unit_id: unit_id, control_root, control_enable_root: false,
    }, "cognition-trial-configuration")?;
    Ok(PreparedTrial { export, control_export, configuration_ref })
}

#[derive(Serialize)]
struct ConditionInstallation<'a> {
    schema_version: u32,
    configuration_ref: &'a ObjectRef,
    condition: Condition,
    root: &'a Path,
    import_outcome: &'a Option<serde_json::Value>,
}

pub(super) fn prepare_condition(
    store: &Store,
    prepared: &PreparedTrial,
    condition: Condition,
    root: &Path,
) -> Result<(Arc<CognitionCatalog>, ObjectRef)> {
    // The caller supplies a fresh UUID path; exclusive creation refuses an
    // existing Node rather than reopening a candidate from an earlier run.
    std::fs::create_dir(root)?;
    let mut node = Node::open(&root.join("node"))?;
    let export = match condition {
        Condition::Candidate => Some(&prepared.export),
        Condition::Control => prepared.control_export.as_ref(),
    };
    let import_outcome = export.map(|export| node.handle_unit_request(UnitRequest::Import {
        export: export.clone(),
        package_root: root.join("cognition").to_string_lossy().into_owned(),
        enable: Some(condition == Condition::Candidate),
    })).transpose()?;
    // Import's actual resolutions retain any missing candidate dependency
    // boundary in control; a filtered closure is not a claim of equivalence.
    let installation_ref = seal_trial_object(store, &ConditionInstallation {
        schema_version: 1, configuration_ref: &prepared.configuration_ref,
        condition, root, import_outcome: &import_outcome,
    }, "cognition-trial-installation")?;
    Ok((Arc::new(CognitionCatalog::new(Arc::new(parking_lot::Mutex::new(node)))), installation_ref))
}
