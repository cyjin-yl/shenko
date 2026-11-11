//! SelfKnowledgeProvider — docs/specs/cognition-units.md#spec-38
//! Built-in, replaceable self-explanation plugin.
//! Provides self:// resources: runtime, docs, source, design.
//! Binds running artifact → source revision → docs → design philosophy.
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{info, debug};
use shenko_types::capabilities::ChannelCapabilities;
use shenko_types::ids::EntityId;
use shenko_types::agent::AgentCapability;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::path::{Component, Path};

/// Maximum number of files the source index covers.
const MAX_INDEXED_FILES: usize = 160;
/// Maximum bytes of one indexed file.
const MAX_INDEXED_FILE_BYTES: usize = 64 * 1024;

/// Resource entry types — self://
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfResourceKind {
    Runtime,   // self://runtime — active generation, plugins, hooks, provider capabilities
    Docs,      // self://docs — user manual, plugin dev guide, command help
    Source,    // self://source — actual source code, public interfaces, build recipes
    Design,    // self://design — SPEC, design philosophy, architecture decisions
}

impl SelfResourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SelfResourceKind::Runtime => "runtime",
            SelfResourceKind::Docs => "docs",
            SelfResourceKind::Source => "source",
            SelfResourceKind::Design => "design",
        }
    }

    /// Parse the serialized kind name; unknown kinds are rejected so a misspelt
    /// filter is an explicit failure, not a silently empty list.
    pub fn from_str(value: &str) -> Option<Self> {
        match value.trim() {
            "runtime" => Some(SelfResourceKind::Runtime),
            "docs" => Some(SelfResourceKind::Docs),
            "source" => Some(SelfResourceKind::Source),
            "design" => Some(SelfResourceKind::Design),
            _ => None,
        }
    }
}

/// A self-knowledge resource record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfResource {
    pub resource_id: EntityId,
    pub kind: SelfResourceKind,
    pub uri: String,             // e.g. "self://runtime/plugins"
    pub title: String,
    pub content: String,
    pub source_revision: Option<String>,  // git SHA of source this was built from
    pub artifact_digest: Option<String>,  // digest of the running artifact
    pub doc_revision: Option<String>,     // documentation version
    pub applicable_version: Option<String>,
    pub cached_at: DateTime<Utc>,
    pub status: ResourceStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceStatus {
    Current,       // verified against running generation
    Stale,         // needs reindex
    Candidate,     // from a candidate build, not yet active
    Unknown,
}

impl ResourceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResourceStatus::Current => "current",
            ResourceStatus::Stale => "stale",
            ResourceStatus::Candidate => "candidate",
            ResourceStatus::Unknown => "unknown",
        }
    }
}

/// SelfKnowledgeProvider — answers "how do I work, why this way, is X enabled"
pub struct SelfKnowledgeProvider {
    resources: RwLock<Vec<SelfResource>>,
    build_manifest: RwLock<BuildManifest>,
    /// The native finite-child definitions this instance actually registered.
    native_definitions: RwLock<Vec<(String, u32)>>,
    /// The cognition units this instance actually installed.
    cognition_units: RwLock<Vec<crate::cognition::CognitionUnit>>,
    /// The live agent-visible capability catalog of the runtime registry.
    agent_capabilities: RwLock<Vec<AgentCapability>>,
    /// What every installed channel can really do, as its plugin declared it.
    channel_capabilities: RwLock<Vec<(String, String, ChannelCapabilities)>>,
}

/// Build manifest — binds running artifact to source/docs/design
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildManifest {
    pub manifest_id: EntityId,
    pub generation: String,           // RuntimeGeneration
    pub source_revision: String,      // git SHA
    pub artifact_digest: String,      // build artifact digest
    pub spec_version: String,         // version of the domain contracts
    pub plugin_lock: Vec<PluginLockEntry>,
    pub data_schema_version: u32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginLockEntry {
    pub plugin_id: EntityId,
    pub name: String,
    pub version: String,
    pub digest: String,
}

impl SelfKnowledgeProvider {
    pub fn new(source_revision: &str, spec_version: &str) -> Self {
        let manifest = BuildManifest {
            manifest_id: EntityId::new(),
            generation: "seed".to_string(),
            source_revision: source_revision.to_string(),
            artifact_digest: String::new(),
            spec_version: spec_version.to_string(),
            plugin_lock: vec![],
            data_schema_version: 1,
            created_at: Utc::now(),
        };

        let provider = Self {
            resources: RwLock::new(vec![]),
            build_manifest: RwLock::new(manifest),
            native_definitions: RwLock::new(vec![]),
            cognition_units: RwLock::new(vec![]),
            agent_capabilities: RwLock::new(vec![]),
            channel_capabilities: RwLock::new(vec![]),
        };
        provider.seed_default_resources();
        provider
    }

    /// Seed default self-knowledge resources. docs/specs/cognition-units.md#spec-38-3
    fn seed_default_resources(&self) {
        let now = Utc::now();
        let revision = self.build_manifest.read().source_revision.clone();

        let defaults = vec![
            SelfResource {
                resource_id: EntityId::new(),
                kind: SelfResourceKind::Design,
                uri: "self://design/philosophy".into(),
                title: "设计哲学".into(),
                content: "Shenko 是面向 personal agent 的最小框架。编码、聊天、研究、远程控制和电子产品可组合。\
                         永续主体是一条真正持续的 thought loop，不因任务为空而停止。\
                         一切插件化，核心不编入业务逻辑。".into(),
                source_revision: Some(revision.clone()),
                artifact_digest: None,
                doc_revision: Some("0.6.2".into()),
                applicable_version: Some("0.6.2".into()),
                cached_at: now,
                status: ResourceStatus::Current,
            },
            SelfResource {
                resource_id: EntityId::new(),
                kind: SelfResourceKind::Docs,
                uri: "self://docs/plugin-dev".into(),
                title: "插件开发指南".into(),
                content: "插件通过 cordis-rs backbone 注册。实现 Plugin trait (meta/start/stop)。\
                         提供 capability 声明。effect 合同按 docs/specs/durable-state.md#spec-36 声明：\
                         reset/restore/replay/cancel/compensate 五种区分。".into(),
                source_revision: Some(revision.clone()),
                artifact_digest: None,
                doc_revision: Some("0.6.2".into()),
                applicable_version: Some("0.6.2".into()),
                cached_at: now,
                status: ResourceStatus::Current,
            },
            SelfResource {
                resource_id: EntityId::new(),
                kind: SelfResourceKind::Source,
                uri: "self://source/layout".into(),
                title: "源码结构".into(),
                content: "crates/shenko-types (协议类型), shenko-store (SQLite+CAS), \
                         shenko-kernel (插件注册+生命周期), shenko-broker (推理调度), \
                         shenko-subject (持续主体), shenko-memory, shenko-channel-napcat, \
                         shenko-exec-host (有限 native child：host.printf/history.query/self.knowledge.query), \
                         shenko-exec-ssh (SSH+tmux seat), shenko-wasm, \
                         shenko-repl (WASI REPL sessions + R0 journal), \
                         shenko-compact-budget, shenko-compact-snap, \
                         shenko-awareness-core (觉察单元+自解释), shenko-node (安装单元/节点), \
                         shenko-quota, shenko-context-policy, shenko-ui, shenko-seed".into(),
                source_revision: Some(revision),
                artifact_digest: None,
                doc_revision: None,
                applicable_version: None,
                cached_at: now,
                status: ResourceStatus::Current,
            },
        ];

        let count = defaults.len();
        self.resources.write().extend(defaults);
        info!("self-knowledge provider seeded with {} resources", count);
    }

    /// Query self-knowledge by URI or keyword. docs/specs/cognition-units.md#spec-38-1
    pub fn query(&self, query: &str) -> Vec<SelfResource> {
        let resources = self.resources.read();
        let q = query.to_lowercase();
        resources.iter().filter(|r| {
            r.uri.to_lowercase().contains(&q)
                || r.title.to_lowercase().contains(&q)
                || r.content.to_lowercase().contains(&q)
        }).cloned().collect()
    }

    /// Get resource by URI
    pub fn get(&self, uri: &str) -> Option<SelfResource> {
        self.resources.read().iter().find(|r| r.uri == uri).cloned()
    }

    /// List all resources of a kind
    pub fn list(&self, kind: SelfResourceKind) -> Vec<SelfResource> {
        self.resources.read().iter().filter(|r| r.kind == kind).cloned().collect()
    }

    /// Install or update a resource. docs/specs/cognition-units.md#spec-38-6
    pub fn install_resource(&self, resource: SelfResource) {
        let mut resources = self.resources.write();
        // Replace if same URI exists
        if let Some(existing) = resources.iter_mut().find(|r| r.uri == resource.uri) {
            *existing = resource;
        } else {
            resources.push(resource);
        }
    }

    /// Record what this instance actually installed — docs/specs/cognition-units.md#spec-38-3, R124.
    ///
    /// `self://runtime/capabilities` is built from the real registries at
    /// startup, not from a template: `native_definitions` are the registered
    /// finite-child `(kind, version)` pairs and `cognition_units` the installed
    /// cognition units with version and state. The live agent-visible
    /// capability catalog is added separately by
    /// [`SelfKnowledgeProvider::install_agent_capabilities`]; all three
    /// sections are rendered into the same resource. A capability absent
    /// from this snapshot is absent from the running instance; the caller must
    /// not blend registry facts with a static product feature list.
    pub fn install_runtime_capabilities(
        &self,
        native_definitions: &[(String, u32)],
        cognition_units: &[crate::cognition::CognitionUnit],
    ) {
        *self.native_definitions.write() = native_definitions.to_vec();
        *self.cognition_units.write() = cognition_units.to_vec();
        self.render_runtime_capabilities();
    }

    /// Record the live agent-visible capability catalog — docs/specs/cognition-units.md#spec-38-3.
    ///
    /// This is the runtime registry's own snapshot of what this agent can
    /// actually use right now: the capability ids it advertises, their
    /// versions, what they do and whether they are enabled. It is kept
    /// separate from the native-definition and cognition-unit sections so a
    /// later registry refresh does not have to re-read the other two.
    pub fn install_agent_capabilities(&self, capabilities: &[AgentCapability]) {
        *self.agent_capabilities.write() = capabilities.to_vec();
        self.render_runtime_capabilities();
    }

    /// Refresh the installed-unit section after a runtime change.
    ///
    /// Installing, enabling or judging a unit changes what this instance
    /// offers. The self-knowledge view is rebuilt from the installation
    /// authority again, so the answer about "is it enabled" is the live fact
    /// and not the snapshot from startup.
    pub fn refresh_cognition_units(&self, cognition_units: &[crate::cognition::CognitionUnit]) {
        *self.cognition_units.write() = cognition_units.to_vec();
        self.render_runtime_capabilities();
    }

    /// Record what every installed channel can really do — docs/specs/channel-adapters.md#spec-15-3, docs/specs/channels.md#spec-57-4.
    ///
    /// This is the same kind of registry snapshot as the sections above: the
    /// table each installed plugin declared, with the account it serves. A
    /// channel absent from this list is not installed, and a field that is
    /// `false` is a promise the platform does not keep — so the subject can
    /// read what it may actually attempt before it promises something.
    pub fn install_channel_capabilities(
        &self,
        capabilities: &[(String, String, ChannelCapabilities)],
    ) {
        *self.channel_capabilities.write() = capabilities.to_vec();
        self.render_runtime_capabilities();
    }

    /// Rebuild `self://runtime/capabilities` from the recorded registries.
    ///
    /// Every section is a registry snapshot of this instance, never a static
    /// product feature list: a capability absent from this text is absent
    /// from the running instance.
    fn render_runtime_capabilities(&self) {
        let native_definitions = self.native_definitions.read().clone();
        let cognition_units = self.cognition_units.read().clone();
        let agent_capabilities = self.agent_capabilities.read().clone();
        let channel_capabilities = self.channel_capabilities.read().clone();
        let mut content = String::from(
            "本实例启动时实际注册的能力（注册表快照；不是产品全量功能清单，\
            未列出的能力不存在于当前实例）。\n\n\
            已注册的 native definitions（用 [delegate:kind:version] 委派有限 child）：",
        );
        if native_definitions.is_empty() {
            content.push_str("\n- （无）");
        }
        for (kind, version) in native_definitions {
            content.push_str(&format!("\n- {kind}:{version}"));
        }
        content.push_str("\n\n已安装的认知单元（安装权威：Node 安装单元）：");
        if cognition_units.is_empty() {
            content.push_str("\n- （无）");
        }
        for unit in cognition_units {
            let state = unit.state.as_str();
            content.push_str(&format!(
                "\n- {}@{}（{state}）— {}",
                unit.unit_id, unit.revision, unit.capability_card
            ));
        }
        content.push_str("\n\n实时 agent 能力目录（运行时注册表快照；不是产品全量功能清单）：");
        if agent_capabilities.is_empty() {
            content.push_str("\n- （无）");
        }
        for capability in &agent_capabilities {
            let state = if capability.enabled { "启用" } else { "停用" };
            content.push_str(&format!(
                "\n- {}@{}（{state}）— {}",
                capability.capability_id, capability.version, capability.description
            ));
        }
        content.push_str("\n\n已安装通道的真实能力（插件自己的声明；false 表示不承诺该动作，不是平台也许接受）：");
        if channel_capabilities.is_empty() {
            content.push_str("\n- （无）");
        }
        for (channel, account, capabilities) in &channel_capabilities {
            let mut offered: Vec<&str> = Vec::new();
            if capabilities.direct {
                offered.push("单聊");
            }
            if capabilities.group {
                offered.push("群聊");
            }
            if capabilities.thread {
                offered.push("话题");
            }
            if capabilities.guild_channel {
                offered.push("频道");
            }
            if capabilities.rich_text {
                offered.push("富文本");
            }
            if capabilities.attachments {
                offered.push("附件");
            }
            if capabilities.edit {
                offered.push("编辑");
            }
            if capabilities.retract {
                offered.push("撤回");
            }
            if capabilities.typing {
                offered.push("正在输入");
            }
            if capabilities.proactive {
                offered.push("主动投递");
            }
            content.push_str(&format!(
                "\n- {channel}（{account}）：{}",
                if offered.is_empty() {
                    "无可承诺的动作".to_string()
                } else {
                    offered.join("、")
                }
            ));
            for note in &capabilities.notes {
                content.push_str(&format!("\n    - {note}"));
            }
        }
        let revision = self.build_manifest.read().source_revision.clone();
        self.install_resource(SelfResource {
            resource_id: EntityId::new(),
            kind: SelfResourceKind::Runtime,
            uri: "self://runtime/capabilities".into(),
            title: "当前实例已安装能力清单".into(),
            content,
            source_revision: Some(revision),
            artifact_digest: None,
            doc_revision: None,
            applicable_version: None,
            cached_at: Utc::now(),
            status: ResourceStatus::Current,
        });
    }

    /// Invalidate cached resources when generation changes. docs/specs/cognition-units.md#spec-38-6
    pub fn invalidate_on_generation_change(&self, new_generation: &str) {
        let mut resources = self.resources.write();
        for r in resources.iter_mut() {
            if r.status == ResourceStatus::Current {
                r.status = ResourceStatus::Stale;
            }
        }
        let mut manifest = self.build_manifest.write();
        manifest.generation = new_generation.to_string();
        debug!("self-knowledge invalidated for generation {}", new_generation);
    }

    /// Publish the build manifest as a readable runtime resource.
    ///
    /// The manifest binds the running artifact to its source revision, its
    /// plugin lock and its data schema. It is a runtime fact, so it is
    /// published with the generation it belongs to and invalidated with it.
    pub fn install_manifest_resource(&self) {
        let manifest = self.manifest();
        let content = serde_json::to_string_pretty(&manifest).unwrap_or_default();
        self.install_resource(SelfResource {
            resource_id: EntityId::new(),
            kind: SelfResourceKind::Runtime,
            uri: "self://runtime/manifest".into(),
            title: "Running build manifest".into(),
            content,
            source_revision: Some(manifest.source_revision.clone()),
            artifact_digest: Some(manifest.artifact_digest.clone()),
            doc_revision: None,
            applicable_version: Some(manifest.spec_version.clone()),
            cached_at: Utc::now(),
            status: ResourceStatus::Current,
        });
    }

    /// Index the actual files of the running artifact.
    ///
    /// The index is built from the files that exist on this node, not from a
    /// description of them. Every entry keeps the path it came from, the
    /// revision it was read at and the digest of its bytes, so an answer can
    /// be traced back to a real file. When the source tree is not present the
    /// index says exactly that instead of describing a repository that is
    /// not here.
    pub fn install_source_index(&self, root: &Path, revision: &str) -> anyhow::Result<usize> {
        // Resolve the current documentation before replacing the old index.
        // An unreadable or invalid manifest is not evidence that the old root
        // documents still describe the running tree.
        let mut entries = if root.is_dir() {
            collect_current_documents(root)?
        } else {
            Vec::new()
        };
        let mut resources = self.resources.write();
        // A re-index replaces the previous file resources instead of keeping
        // a second copy of the same tree.
        resources.retain(|resource| {
            resource.uri != "self://source/layout"
                && !resource.uri.starts_with("self://source/files/")
                && !resource.uri.starts_with("self://docs/files/")
                && !resource.uri.starts_with("self://design/files/")
        });
        if !root.is_dir() {
            resources.push(SelfResource {
                resource_id: EntityId::new(),
                kind: SelfResourceKind::Source,
                uri: "self://source/layout".into(),
                title: "Source tree location".into(),
                content: format!("This running instance has no source tree at {}. A feature that exists in the repository main branch is not a capability of this instance.", root.display()),
                source_revision: Some(revision.to_string()),
                artifact_digest: None,
                doc_revision: None,
                applicable_version: None,
                cached_at: Utc::now(),
                status: ResourceStatus::Unknown,
            });
            return Ok(0);
        }
        collect_indexed(root, &root.join("docs"), &mut entries);
        collect_indexed(root, &root.join(".agents").join("skills"), &mut entries);
        collect_crate_entries(root, &mut entries);
        let mut indexed = 0usize;
        let mut seen = HashSet::new();
        for (relative, kind, absolute) in entries {
            if !seen.insert(relative.clone()) {
                continue;
            }
            if indexed >= MAX_INDEXED_FILES {
                break;
            }
            let Ok(bytes) = std::fs::read(&absolute) else {
                continue;
            };
            let truncated = bytes.len() > MAX_INDEXED_FILE_BYTES;
            let slice = if truncated { &bytes[..MAX_INDEXED_FILE_BYTES] } else { &bytes[..] };
            let digest = shenko_types::object::Digest::blake3(slice);
            let mut content = String::from_utf8_lossy(slice).into_owned();
            if truncated {
                content.push_str("\n(this file is longer than the index bound; the full content lives in the source tree)");
            }
            let section = match kind {
                SelfResourceKind::Docs => "docs",
                SelfResourceKind::Design => "design",
                _ => "source",
            };
            resources.push(SelfResource {
                resource_id: EntityId::new(),
                kind,
                uri: format!("self://{section}/files/{relative}"),
                title: relative.clone(),
                content,
                source_revision: Some(revision.to_string()),
                artifact_digest: Some(digest.0),
                doc_revision: None,
                applicable_version: None,
                cached_at: Utc::now(),
                status: ResourceStatus::Current,
            });
            indexed += 1;
        }
        resources.push(SelfResource {
            resource_id: EntityId::new(),
            kind: SelfResourceKind::Source,
            uri: "self://source/layout".into(),
            title: "Files this instance can read".into(),
            content: format!("This instance indexed {indexed} real files at revision {revision}. Every self://source/files/... resource carries its own content digest."),
            source_revision: Some(revision.to_string()),
            artifact_digest: None,
            doc_revision: None,
            applicable_version: None,
            cached_at: Utc::now(),
            status: ResourceStatus::Current,
        });
        Ok(indexed)
    }

    /// Update build manifest
    pub fn update_manifest(&self, manifest: BuildManifest) {
        *self.build_manifest.write() = manifest;
    }

    /// Get current build manifest
    pub fn manifest(&self) -> BuildManifest {
        self.build_manifest.read().clone()
    }
}

/// The migration manifest identifies current domain text, not its archived
/// sources. Only the fields needed for discovery belong in this reader.
#[derive(Deserialize)]
struct ContractDocuments {
    schema_version: u32,
    chunks: Vec<ContractDocument>,
}

#[derive(Deserialize)]
struct ContractDocument {
    path: String,
}

fn historical_document(path: &Path) -> bool {
    path.starts_with("docs/history") || path.starts_with("docs/research/archive")
}

/// Put current contracts before supplementary documents so historical files
/// cannot consume the bounded index before the design material is reached.
fn collect_current_documents(
    root: &Path,
) -> anyhow::Result<Vec<(String, SelfResourceKind, String)>> {
    use anyhow::Context;

    let manifest_path = root.join("docs/contract-migration.json");
    let bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("cannot read current contract manifest {}", manifest_path.display()))?;
    let manifest: ContractDocuments = serde_json::from_slice(&bytes)
        .context("invalid current contract manifest")?;
    anyhow::ensure!(manifest.schema_version == 1, "unsupported current contract manifest schema");
    anyhow::ensure!(!manifest.chunks.is_empty(), "current contract manifest has no document targets");

    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for name in ["AGENTS.md", "README.md", "docs/INDEX.md"] {
        let path = root.join(name);
        if path.is_file() {
            seen.insert(name.to_string());
            entries.push((name.to_string(), SelfResourceKind::Docs, path.to_string_lossy().into_owned()));
        }
    }
    for chunk in manifest.chunks {
        let relative = Path::new(&chunk.path);
        anyhow::ensure!(
            relative.starts_with("docs")
                && relative.components().all(|component| matches!(component, Component::Normal(_)))
                && relative.extension().is_some_and(|extension| extension == "md")
                && !historical_document(relative),
            "invalid current contract target: {}", chunk.path
        );
        if seen.insert(chunk.path.clone()) {
            let path = root.join(relative);
            entries.push((chunk.path, SelfResourceKind::Design, path.to_string_lossy().into_owned()));
        }
    }
    Ok(entries)
}

/// Collect supplementary documents, excluding historical material that must
/// not be presented as the current design of the running artifact.
fn collect_indexed(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<(String, SelfResourceKind, String)>,
) {
    if !dir.is_dir() {
        return;
    }
    let mut stack = vec![dir.to_path_buf()];
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    while let Some(current) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in read.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.strip_prefix(root).is_ok_and(historical_document) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_some_and(|extension| extension == "md") {
                files.push(path);
            }
        }
    }
    files.sort();
    for path in files {
        let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
        entries.push((relative, SelfResourceKind::Docs, path.to_string_lossy().into_owned()));
    }
}

/// Collect the crate entry points of the source tree.
fn collect_crate_entries(root: &Path, entries: &mut Vec<(String, SelfResourceKind, String)>) {
    let crates_dir = root.join("crates");
    let Ok(read) = std::fs::read_dir(&crates_dir) else {
        return;
    };
    let mut dirs: Vec<std::path::PathBuf> = read
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        for file in ["lib.rs", "main.rs"] {
            let path = dir.join("src").join(file);
            if path.is_file() {
                let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
                entries.push((relative, SelfResourceKind::Source, path.to_string_lossy().into_owned()));
            }
        }
    }
}
