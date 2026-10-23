//! Plugin trait and registry — wraps cordis-rs generation model.
//! SPEC §7: "plugin化要有真实依赖和生命周期边界"
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

/// Plugin metadata.
#[derive(Debug, Clone)]
pub struct PluginMeta {
    pub id: PluginId,
    pub name: String,
    pub version: String,
    pub capabilities_provided: Vec<String>,
    pub capabilities_required: Vec<String>,
}

/// Plugin lifecycle trait.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    fn meta(&self) -> &PluginMeta;
    async fn start(&self) -> Result<()>;
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
