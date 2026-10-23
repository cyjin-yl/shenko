//! Plugin trait and registry — wraps cordis-rs generation model.
//! SPEC §7: "plugin化要有真实依赖和生命周期边界"
//!
//! Aligned with cordis-rs rev 1754062 Plugin/Context/Service pattern:
//!   - Config/Input/PrepareError/ApplyError associated types on the cordis trait
//!   - prepare (sync validation) + apply (async, non-blocking poll) lifecycle
//!   - Context: cheap immutable runtime view, generation identity, fiber owner
//!   - generation-owned effect cleanup obligations; drain in reverse-commit order
//!
//! The shenko Plugin trait keeps meta/start/stop for backward compatibility with
//! existing impls. generation() and effect_cleanup() carry default no-ops so
//! existing impls continue to compile unchanged.
use std::sync::Arc;
use async_trait::async_trait;
use shenko_types::ids::EntityId;
use anyhow::Result;

/// Plugin identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginId(pub EntityId);

impl PluginId {
    pub fn new() -> Self { Self(EntityId::new()) }
}

impl Default for PluginId {
    fn default() -> Self { Self::new() }
}

/// Plugin metadata.
#[derive(Debug, Clone)]
pub struct PluginMeta {
    pub id: PluginId,
    pub name: String,
    pub version: String,
    pub capabilities_provided: Vec<String>,
    pub capabilities_required: Vec<String>,
}

/// Cheap, clone-on-copy view into a plugin's runtime context.
///
/// Mirrors cordis-rs Context: carries a generation counter and a
/// cancellation-aware fiber identity. Passed to start/apply so plugins
/// can register generation-owned cleanup obligations.
#[derive(Debug, Clone)]
pub struct PluginContext {
    /// Monotonically increasing generation for this fiber.
    /// Advances each time the plugin is restarted or its input is swapped.
    pub generation: u64,
    /// Stable fiber identity across generations.
    pub fiber_id: EntityId,
}

impl PluginContext {
    pub fn new(generation: u64) -> Self {
        Self { generation, fiber_id: EntityId::new() }
    }
}

/// Service interface marker — mirrors cordis-rs Service.
///
/// A service is a named, optionally configurable capability a plugin
/// declares it needs (via PluginMeta::capabilities_required) or provides
/// (via PluginMeta::capabilities_provided). The runtime resolves the
/// binding before the plugin's start/apply is called.
pub trait PluginService: Send + Sync + 'static {
    /// Canonical service name used for dependency resolution.
    const NAME: &'static str;
}

/// Plugin lifecycle trait — backward-compatible shenko facade over
/// the cordis-rs Plugin/Context/Service pattern.
///
/// Existing impls only need meta/start/stop. The new methods carry
/// default no-op implementations so they are non-breaking.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    // --- Identity and metadata (shenko-native) ---

    fn meta(&self) -> &PluginMeta;

    // --- cordis-rs aligned: generation tracking ---

    /// Return the current generation counter for this plugin instance.
    ///
    /// Mirrors cordis-rs Context's generation identity: increments each
    /// time the plugin is restarted or receives a new Input via update.
    /// Used by the supervisor to detect stale effect registrations.
    fn generation(&self) -> u64 {
        0
    }

    // --- cordis-rs aligned: effect cleanup ---

    /// Run generation-owned cleanup obligations in reverse-commit order.
    ///
    /// Mirrors cordis-rs effect.rs: called by the supervisor when this
    /// generation closes (stop, restart, update, or failure). Each
    /// registered side effect (listener, service publication, task,
    /// resource handle) should be cleaned up here exactly once.
    ///
    /// The default no-op is correct for plugins that manage no external
    /// effects. Override to drain effect registrations in the order they
    /// should be reversed.
    async fn effect_cleanup(&self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }

    // --- Lifecycle (shenko-native, kept for backward compat) ---

    /// Start the plugin in the given context.
    ///
    /// Equivalent to cordis-rs apply: keep each poll non-blocking.
    /// Move blocking work to spawn_blocking. Registers generation-owned
    /// effects that will be drained by effect_cleanup when the
    /// generation closes.
    async fn start(&self) -> Result<()>;

    /// Stop the plugin, draining generation-owned effects.
    ///
    /// Called by the supervisor in reverse-registration order, mirroring
    /// cordis-rs generation drain on unload/dispose.
    async fn stop(&self) -> Result<()>;
}

/// Plugin registry — tracks installed plugins.
pub struct PluginRegistry {
    plugins: parking_lot::RwLock<Vec<Arc<dyn Plugin>>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self { plugins: parking_lot::RwLock::new(vec![]) }
    }

    pub fn register(&self, plugin: Arc<dyn Plugin>) {
        self.plugins.write().push(plugin);
    }

    pub fn list(&self) -> Vec<Arc<dyn Plugin>> {
        self.plugins.read().clone()
    }

    pub async fn start_all(&self) -> Result<()> {
        let plugins = self.list();
        for p in plugins {
            p.start().await?;
        }
        Ok(())
    }

    pub async fn stop_all(&self) -> Result<()> {
        let plugins = self.list();
        for p in plugins.iter().rev() {
            if let Err(e) = p.stop().await {
                tracing::warn!(plugin = %p.meta().name, err = %e, "stop error");
            }
        }
        Ok(())
    }
}

impl Default for PluginRegistry {
    fn default() -> Self { Self::new() }
}
