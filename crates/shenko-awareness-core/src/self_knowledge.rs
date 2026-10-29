//! SelfKnowledgeProvider — SPEC §38
//! Built-in, replaceable self-explanation plugin.
//! Provides self:// resources: runtime, docs, source, design.
//! Binds running artifact → source revision → docs → design philosophy.
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{info, debug};
use shenko_types::ids::EntityId;
use chrono::{DateTime, Utc};

/// Resource entry types — self://
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfResourceKind {
    Runtime,   // self://runtime — active generation, plugins, hooks, provider capabilities
    Docs,      // self://docs — user manual, plugin dev guide, command help
    Source,    // self://source — actual source code, public interfaces, build recipes
    Design,    // self://design — SPEC, design philosophy, architecture decisions
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

/// SelfKnowledgeProvider — answers "how do I work, why this way, is X enabled"
pub struct SelfKnowledgeProvider {
    resources: RwLock<Vec<SelfResource>>,
    build_manifest: RwLock<BuildManifest>,
}

/// Build manifest — binds running artifact to source/docs/design
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildManifest {
    pub manifest_id: EntityId,
    pub generation: String,           // RuntimeGeneration
    pub source_revision: String,      // git SHA
    pub artifact_digest: String,      // build artifact digest
    pub spec_version: String,         // SPEC.md version
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
        };
        provider.seed_default_resources();
        provider
    }

    /// Seed default self-knowledge resources. SPEC §38.3
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
                kind: SelfResourceKind::Runtime,
                uri: "self://runtime/capabilities".into(),
                title: "当前能力清单".into(),
                content: "Store (SQLite WAL), CAS (blake3), Broker (Anthropic API), \
                         Subject loop (S0/S1), NapCat (OneBot v11 WS), \
                         WebUI (axum), Tmux seat, SSH exec (localhost only for now), \
                         WASM runtime (stub — not yet implemented), \
                         Memory, Awareness, Budget/Snap compact; \
                         SelfKnowledge: capabilities are partially hardcoded; runtime probing not yet implemented".into(),
                source_revision: Some(revision.clone()),
                artifact_digest: None,
                doc_revision: None,
                applicable_version: None,
                cached_at: now,
                status: ResourceStatus::Current,
            },
            SelfResource {
                resource_id: EntityId::new(),
                kind: SelfResourceKind::Docs,
                uri: "self://docs/plugin-dev".into(),
                title: "插件开发指南".into(),
                content: "插件通过 cordis-rs backbone 注册。实现 Plugin trait (meta/start/stop)。\
                         提供 capability 声明。effect 合同按 §36 声明：\
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
                         shenko-exec-ssh (SSH+tmux seat), shenko-wasm, \
                         shenko-repl (WASI REPL sessions + R0 journal), \
                         shenko-compact-budget, shenko-compact-snap, \
                         shenko-awareness-core (自解释), shenko-ui, shenko-seed".into(),
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

    /// Query self-knowledge by URI or keyword. SPEC §38.1
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

    /// Install or update a resource. SPEC §38.6
    pub fn install_resource(&self, resource: SelfResource) {
        let mut resources = self.resources.write();
        // Replace if same URI exists
        if let Some(existing) = resources.iter_mut().find(|r| r.uri == resource.uri) {
            *existing = resource;
        } else {
            resources.push(resource);
        }
    }

    /// Invalidate cached resources when generation changes. SPEC §38.6
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

    /// Update build manifest
    pub fn update_manifest(&self, manifest: BuildManifest) {
        *self.build_manifest.write() = manifest;
    }

    /// Get current build manifest
    pub fn manifest(&self) -> BuildManifest {
        self.build_manifest.read().clone()
    }
}
