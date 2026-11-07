//! Ordinary Node packages for independently installed compatibility plugins.
//! This helper neither registers plugins nor knows any external ecosystem.

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use shenko_node::{DependencySelector, DependencySpec, DependencyStrength, InstalledUnitFact,
    Node, UNIT_SCHEMA, UnitDescriptor, UnitExport, UnitKind, UnitPackage};
use shenko_types::object::Digest;

pub fn plugin_package(unit_id: &str) -> Result<UnitPackage> {
    let artifact = serde_json::to_vec(&json!({"format":"shenko-compat-plugin", "schema_version":1,
        "implementation":unit_id, "version":"1", "external_services_included":false}))?;
    Ok(UnitPackage {
        unit: UnitDescriptor {
            unit_id: unit_id.into(), kind: UnitKind::Code, revision: "1".into(), artifact_digest: Digest::blake3(&artifact),
            provides: vec![unit_id.into()], dependencies: Vec::new(), applicability: None,
            config: json!({"compat":{"format":"plugin","implementation":unit_id}}),
        }, artifact_base64: STANDARD.encode(artifact), evidence: Vec::new(), resolved_dependencies: None,
    })
}

pub fn plugin_export(unit_id: &str, enabled: bool) -> Result<UnitExport> {
    let package = plugin_package(unit_id)?;
    Ok(UnitExport { schema: UNIT_SCHEMA, root_unit_id: unit_id.into(), root_revision: Some(package.unit.revision.clone()),
        root_artifact_digest: Some(package.unit.artifact_digest.clone()), origin_node: None, enabled, packages: vec![package] })
}

pub fn plugin_dependency(unit_id: &str) -> Result<DependencySpec> {
    let package = plugin_package(unit_id)?;
    Ok(DependencySpec { edge_id: "compatibility-plugin".into(), strength: DependencyStrength::Required,
        selector: DependencySelector::Exact { unit_id: package.unit.unit_id, revision_requirement: Some(package.unit.revision),
            artifact_digest: Some(package.unit.artifact_digest), contract_requirement: None },
        purpose: Some("Separately installed compatibility plugin; not the third-party implementation".into()) })
}

/// Exact installation identity, also retained in the frozen operation and its
/// learning receipt. Human-readable upstream labels do not replace this hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitRef {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
}

/// A revision label derived from content, bounded to the Node's revision field.
///
/// The full canonical hexadecimal digest fits the 64-byte revision field.
/// Keep every bit: a changed resource bundle must have a distinct revision
/// even when its unchanged entry file still has the same artifact digest.
pub fn content_revision(digest: &Digest) -> Result<String> {
    let hex = digest
        .canonical_blake3_hex()
        .context("content identity is not a canonical digest")?;
    Ok(hex.to_owned())
}

pub fn plugin_enabled(node: &Node, plugin_id: &str) -> Result<bool> {
    let Some(fact) = node.installed_unit_fact(plugin_id)? else { return Ok(false); };
    let expected = plugin_package(plugin_id)?.unit;
    Ok(fact.enabled && fact.revision == expected.revision && fact.artifact_digest == expected.artifact_digest)
}

impl UnitRef {
    /// This exact version is retained by the installation.
    ///
    /// Byte identity comes from `unit_content`, which re-hashes the object, so
    /// a label cannot select a different body. A retained-but-disabled version
    /// is still inspectable locally; using it *as* an adapter is a separate
    /// decision, made by `enabled_exact` and `installed`.
    pub fn exact(&self, node: &Node) -> Result<(InstalledUnitFact, Vec<u8>)> {
        let fact = node.retained_unit_fact(&self.unit_id, &self.revision, &self.artifact_digest)?
            .context("exact external unit version is not retained")?;
        let content = node.unit_content(&self.unit_id, &self.revision, Some(&self.artifact_digest))?;
        Ok((fact, content.bytes))
    }

    /// The same check, and the unit is currently offered.
    pub fn enabled_exact(&self, node: &Node) -> Result<(InstalledUnitFact, Vec<u8>)> {
        let (fact, bytes) = self.exact(node)?;
        ensure!(fact.current && fact.enabled, "external unit is not the current enabled version");
        Ok((fact, bytes))
    }

    /// The same check, and the adapter that owns this format is resident.
    pub fn installed(&self, node: &Node, plugin_id: &str) -> Result<(InstalledUnitFact, Vec<u8>)> {
        ensure!(plugin_enabled(node, plugin_id)?, "compatibility plugin is absent, disabled or a different implementation");
        self.enabled_exact(node)
    }
}
