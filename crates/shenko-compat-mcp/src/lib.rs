//! Independently installable MCP protocol compatibility. No third-party
//! server, vendor catalog, account authorization or Skill reader is included.

pub mod native;
mod http;
pub mod protocol;

/// Publishes the native MCP surface only while `compat.mcp` is enabled.
pub use native::McpPlugin;

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use shenko_awareness_core::CognitionMetadata;
use shenko_compat::installation::{plugin_dependency, plugin_package};
use shenko_node::{UNIT_SCHEMA, UnitDescriptor, UnitExport, UnitKind, UnitPackage};
use shenko_types::object::Digest;

pub const UNIT_ID: &str = "compat.mcp";

/// Explicit external connection; credentials remain local environment
/// references, not transferable package contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConnection {
    pub description: String,
    #[serde(default)]
    pub provenance_refs: Vec<String>,
    pub connection: protocol::ConnectionSpec,
}

impl McpConnection {
    pub fn export(&self, unit_id: &str, enabled: bool) -> Result<UnitExport> {
        self.connection.validate()?;
        let bytes = serde_json::to_vec(self)?;
        let digest = Digest::blake3(&bytes);
        let revision = shenko_compat::installation::content_revision(&digest)?;
        let metadata = CognitionMetadata { title: unit_id.into(), capability_card: self.description.clone(),
            provenance_refs: self.provenance_refs.clone(), ..CognitionMetadata::default() };
        Ok(UnitExport { schema: UNIT_SCHEMA, root_unit_id: unit_id.into(), root_revision: Some(revision.clone()),
            root_artifact_digest: Some(digest.clone()), origin_node: None, enabled,
            packages: vec![UnitPackage {
                unit: UnitDescriptor { unit_id: unit_id.into(), kind: UnitKind::Code, revision, artifact_digest: digest,
                    provides: vec![format!("mcp:{unit_id}")], dependencies: vec![plugin_dependency(UNIT_ID)?],
                    config: json!({"cognition":metadata,"compat":{"format":"mcp-connection","schema_version":1}}), applicability: None },
                artifact_base64: STANDARD.encode(bytes), evidence: Vec::new(), resolved_dependencies: None,
            }, plugin_package(UNIT_ID)?] })
    }
}
