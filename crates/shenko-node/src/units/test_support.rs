//! Shared fixtures for the installation responsibility modules.

use crate::Node;
use serde_json::{Value, json};
use shenko_types::object::Digest;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use super::UnitRequest;
use super::codec::{
    Applicability, UNIT_SCHEMA, UnitDescriptor, UnitExport, UnitKind, UnitPackage, b64_encode,
};
use super::resolution::{DependencySelector, DependencySpec, DependencyStrength};

pub(super) fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("shenko-units-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&path)
        .unwrap();
    path
}

pub(super) fn package(
    unit_id: &str,
    revision: &str,
    bytes: &[u8],
    deps: Vec<DependencySpec>,
) -> UnitPackage {
    UnitPackage {
        unit: UnitDescriptor {
            unit_id: unit_id.into(),
            kind: UnitKind::Skill,
            revision: revision.into(),
            artifact_digest: Digest::blake3(bytes),
            provides: vec![],
            dependencies: deps,
            config: json!({"mode": "test"}),
            // A foreign platform label stays advisory.
            applicability: Some(Applicability {
                os: Some("plan9".into()),
                arch: None,
                runtime: None,
                notes: None,
            }),
        },
        artifact_base64: b64_encode(bytes),
        evidence: vec![],
        resolved_dependencies: None,
    }
}

pub(super) fn export(packages: Vec<UnitPackage>) -> UnitExport {
    UnitExport {
        schema: UNIT_SCHEMA,
        root_unit_id: packages[0].unit.unit_id.clone(),
        root_revision: Some(packages[0].unit.revision.clone()),
        root_artifact_digest: Some(packages[0].unit.artifact_digest.clone()),
        origin_node: Some("node-a".into()),
        enabled: false,
        packages,
    }
}

pub(super) fn trial(unit_id: &str, argv: Vec<&str>) -> UnitRequest {
    UnitRequest::Trial {
        unit_id: unit_id.into(),
        argv: argv.into_iter().map(|value| value.to_string()).collect(),
        cwd: None,
        timeout_ms: Some(10_000),
        risk: "runs a trivial interpreter command".into(),
        benefit: "proves the unit can run in this environment".into(),
        consequence: "a failed command leaves the unit unready here".into(),
    }
}

pub(super) fn exact(edge_id: &str, target: &str, digest: Option<Digest>) -> DependencySpec {
    DependencySpec {
        edge_id: edge_id.into(),
        strength: DependencyStrength::Required,
        selector: DependencySelector::Exact {
            unit_id: target.into(),
            revision_requirement: Some("1.0.0".into()),
            artifact_digest: digest,
            contract_requirement: None,
        },
        purpose: None,
    }
}

pub(super) fn import(node: &mut Node, root: &Path, packages: Vec<UnitPackage>) -> Value {
    node.handle_unit_request(UnitRequest::Import {
        export: export(packages),
        package_root: root.to_string_lossy().into_owned(),
        enable: None,
    }).unwrap()
}
