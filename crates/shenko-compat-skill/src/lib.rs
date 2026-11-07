//! Lossless Agent Skills packaging through ordinary installation units.
//! An inline index binds one external CAS artifact; no second registry exists.
use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_awareness_core::CognitionMetadata;
use shenko_compat::installation::{UnitRef, plugin_dependency, plugin_package};
use shenko_node::{DependencySelector, DependencySpec, DependencyStrength, UNIT_SCHEMA,
    UnitArtifactSource, UnitDescriptor, UnitExport, UnitKind, UnitPackage};
use shenko_store::cas::ContentStore;
use shenko_types::object::Digest;

pub mod native;
mod source;
pub use source::{CHUNK_BYTES, MAX_READ_BYTES, SkillSource, SourceFile, SourceIndex, SourceInput};
pub use native::SkillPlugin;
pub const UNIT_ID: &str = "compat.skill";

#[derive(Debug, Clone)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    /// Original declarations, never authority grants or installation requests.
    pub frontmatter: serde_yaml_ng::Value,
}

pub fn validate_member_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 4096
            && !path.contains(['\\', ':', '\0'])
            && path.split('/').all(|part| !part.is_empty() && part != "." && part != ".."),
        "invalid source member path"
    );
    Ok(())
}

pub fn metadata(entry: &[u8]) -> Result<SkillMetadata> {
    let text = std::str::from_utf8(entry).context("SKILL.md is not UTF-8")?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().context("SKILL.md is empty")?;
    ensure!(first.trim_end_matches(['\r', '\n']) == "---", "SKILL.md has no YAML frontmatter");
    let start = first.len();
    let mut end = start;
    let mut closed = false;
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            closed = true;
            break;
        }
        end += line.len();
    }
    ensure!(closed, "SKILL.md has unterminated YAML frontmatter");
    let frontmatter: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text[start..end])?;
    let fields = frontmatter.as_mapping().context("Skill frontmatter must be a mapping")?;
    let field = |key: &str| -> Result<String> {
        let value = fields.get(serde_yaml_ng::Value::String(key.into()))
            .and_then(serde_yaml_ng::Value::as_str)
            .with_context(|| format!("Skill frontmatter needs a string {key}"))?;
        ensure!(!value.trim().is_empty(), "Skill frontmatter {key} is empty");
        Ok(value.to_owned())
    };
    Ok(SkillMetadata { name: field("name")?, description: field("description")?, frontmatter })
}

fn exact_dependency(edge: &str, unit: &UnitDescriptor, purpose: &str) -> DependencySpec {
    DependencySpec { edge_id: edge.into(), strength: DependencyStrength::Required,
        selector: DependencySelector::Exact { unit_id: unit.unit_id.clone(), revision_requirement: Some(unit.revision.clone()),
            artifact_digest: Some(unit.artifact_digest.clone()), contract_requirement: None }, purpose: Some(purpose.into()) }
}

impl SkillSource {
    /// A resource or mode change changes the indexed source revision even when
    /// the entry and upstream version label are unchanged.
    pub fn export(&self, unit_id: &str, enabled: bool) -> Result<UnitExport> {
        self.index.validate()?;
        ensure!(self.index.archive_digest == self.archive.digest && self.index.archive_bytes == self.archive.bytes,
            "Skill archive and index disagree");
        ensure!(Digest::blake3(&self.entry) == self.index.files["SKILL.md"].digest, "Skill entry and index disagree");
        let metadata = metadata(&self.entry)?;
        let index = serde_json::to_vec(&self.index)?;
        ensure!(index.len() <= shenko_node::MAX_ARTIFACT_BYTES, "Skill index exceeds the Node inline bound");
        let source_digest = Digest::blake3(&index);
        let revision = shenko_compat::installation::content_revision(&source_digest)?;
        let source_id = format!("{unit_id}.source");
        ensure!(!unit_id.trim().is_empty() && source_id.len() + ".bytes".len() <= 128, "invalid Skill unit identity");
        let archive = UnitDescriptor {
            unit_id: format!("{source_id}.bytes"), kind: UnitKind::Code, revision: revision.clone(),
            artifact_digest: self.archive.digest.clone(), provides: Vec::new(), dependencies: Vec::new(), applicability: None,
            config: json!({"compat":{"format":"agent-skill-bytes","schema_version":2},
                "external_artifact":{"digest":self.archive.digest,"bytes":self.archive.bytes,"store":"cas","locator":self.archive.locator}}),
        };
        let source = UnitDescriptor {
            unit_id: source_id.clone(), kind: UnitKind::Code, revision: revision.clone(), artifact_digest: source_digest.clone(),
            provides: Vec::new(), dependencies: vec![exact_dependency("original-bytes", &archive, "Original source bytes; local CAS binding is required on each node")],
            config: json!({"compat":{"format":"agent-skill-source","schema_version":2}}), applicability: None,
        };
        let cognition = CognitionMetadata { title: metadata.name, capability_card: metadata.description,
            provenance_refs: self.index.provenance_refs.clone(), ..CognitionMetadata::default() };
        let entry_digest = Digest::blake3(&self.entry);
        let entry = UnitDescriptor {
            unit_id: unit_id.into(), kind: UnitKind::Skill, revision: revision.clone(), artifact_digest: entry_digest.clone(),
            provides: vec![format!("skill:{unit_id}")],
            dependencies: vec![plugin_dependency(UNIT_ID)?, exact_dependency("original-source", &source,
                "Original scripts, references and assets; declarations do not install or authorize dependencies")],
            config: json!({"cognition":cognition,"compat":{"format":"agent-skill","schema_version":2,
                "source_unit_id":source_id,"source_digest":source_digest,"source_revision":revision,"entry":"SKILL.md"}}),
            applicability: None,
        };
        Ok(UnitExport { schema: UNIT_SCHEMA, root_unit_id: unit_id.into(), root_revision: Some(revision),
            root_artifact_digest: Some(entry_digest), origin_node: None, enabled,
            packages: vec![
                UnitPackage { unit: entry, artifact_base64: STANDARD.encode(&self.entry), evidence: Vec::new(), resolved_dependencies: None },
                UnitPackage { unit: source, artifact_base64: STANDARD.encode(index), evidence: Vec::new(), resolved_dependencies: None },
                UnitPackage { unit: archive, artifact_base64: String::new(), evidence: Vec::new(), resolved_dependencies: None },
                plugin_package(UNIT_ID)?,
            ] })
    }
}

fn default_limit() -> u32 { MAX_READ_BYTES }

/// With `path`, offset is a byte position. Without it, offset is a member
/// ordinal; limit bounds serialized names. Both forms return next_offset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceQuery {
    #[serde(default)] pub path: Option<String>,
    #[serde(default)] pub offset: u64,
    #[serde(default = "default_limit")] pub limit: u32,
}

impl Default for SourceQuery {
    fn default() -> Self { Self { path: None, offset: 0, limit: MAX_READ_BYTES } }
}

pub struct InstalledSource {
    pub index: SourceIndex,
    unit: UnitRef,
    offered: bool,
    source_digest: Digest,
}

impl InstalledSource {
    pub fn load(cas: &ContentStore, node: &shenko_node::Node, unit: &UnitRef, offered: bool) -> Result<Self> {
        let (fact, entry) = if offered { unit.enabled_exact(node)? } else { unit.exact(node)? };
        let config = &fact.descriptor.config["compat"];
        ensure!(config["format"] == "agent-skill" && config["schema_version"] == 2, "unit is not an indexed Agent Skill source");
        let source_id = config["source_unit_id"].as_str().context("Skill source identity missing")?;
        let revision = config["source_revision"].as_str().context("Skill source revision missing")?;
        let digest: Digest = serde_json::from_value(config["source_digest"].clone())?;
        let content = node.unit_content(source_id, revision, Some(&digest))?;
        let index: SourceIndex = serde_json::from_slice(&content.bytes)?;
        index.validate()?;
        let archive = node.retained_unit_fact(&format!("{source_id}.bytes"), revision, &index.archive_digest)?
            .context("Skill source archive version is absent")?;
        ensure!(matches!(&archive.artifact_source, UnitArtifactSource::External { store, bytes, .. }
            if store == "cas" && *bytes == index.archive_bytes), "Skill source has no matching external CAS binding");
        let original = index.read(cas, "SKILL.md", 0, shenko_node::MAX_ARTIFACT_BYTES as u32)?;
        ensure!(original == entry && index.files["SKILL.md"].digest == unit.artifact_digest,
            "Skill entry and original source disagree");
        Ok(Self { index, unit: unit.clone(), offered: fact.enabled, source_digest: digest })
    }

    pub fn view(&self, cas: &ContentStore, query: &SourceQuery) -> Result<Value> {
        ensure!(query.limit > 0 && query.limit <= MAX_READ_BYTES, "Skill read limit must be between 1 and {MAX_READ_BYTES}");
        let content = match query.path.as_deref() {
            Some(path) => {
                let bytes = self.index.read(cas, path, query.offset, query.limit)?;
                let file = &self.index.files[path];
                let next = query.offset + bytes.len() as u64;
                json!({"path":path,"executable":file.executable,"file_digest":file.digest,
                    "total_bytes":file.bytes,"offset":query.offset,"offset_unit":"byte",
                    "next_offset":(next < file.bytes).then_some(next),"content_base64":STANDARD.encode(bytes)})
            }
            None => {
                ensure!(query.offset <= self.index.files.len() as u64, "Skill listing offset exceeds member count");
                let mut files = Vec::new();
                let mut remaining = query.limit as usize;
                for path in self.index.files.keys().skip(query.offset as usize) {
                    let size = serde_json::to_vec(path)?.len() + 1;
                    if size > remaining { break; }
                    remaining -= size;
                    files.push(path);
                }
                let next = query.offset + files.len() as u64;
                ensure!(!files.is_empty() || next == self.index.files.len() as u64, "Skill listing limit cannot fit the next member");
                json!({"files":files,"provenance_refs":self.index.provenance_refs,"offset":query.offset,
                    "offset_unit":"member","total_members":self.index.files.len(),
                    "next_offset":(next < self.index.files.len() as u64).then_some(next)})
            }
        };
        Ok(json!({"status":"received","source":self.unit,"offered":self.offered,"source_digest":self.source_digest,
            "archive_digest":self.index.archive_digest,"content":content,
            "scripts_executed":false,"dependency_declarations_are_grants":false}))
    }
}

pub fn source_view(cas: &ContentStore, node: &shenko_node::Node, unit: &UnitRef, query: &SourceQuery, offered: bool) -> Result<Value> {
    InstalledSource::load(cas, node, unit, offered)?.view(cas, query)
}
