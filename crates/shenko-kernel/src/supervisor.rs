//! Minimal host policy around the native Cordis runtime, not a plugin framework.
use anyhow::Result;
use cordis_core::{Context, FiberHandle, Plugin};
use std::sync::Arc;

use crate::capability::CapabilityRegistry;
use crate::lifecycle::LifecycleState;
use crate::plugin::PluginRegistry;
use shenko_types::ids::RuntimeGeneration;

/// Business services are native Plugins with generation-owned Context bindings.
/// The binary's default business graph is not yet installed through this API.
pub struct Supervisor {
    pub lifecycle: Arc<LifecycleState>,
    pub plugins: Arc<PluginRegistry>,
    pub capabilities: Arc<CapabilityRegistry>,
}

impl Supervisor {
    pub fn new(generation: RuntimeGeneration) -> Self {
        let lifecycle = Arc::new(LifecycleState::new(generation));
        Self {
            plugins: Arc::new(PluginRegistry::new(lifecycle.shutdown_token())),
            lifecycle,
            capabilities: Arc::new(CapabilityRegistry::new()),
        }
    }

    pub fn context(&self) -> &Context {
        self.plugins.context()
    }

    pub async fn spawn<P: Plugin>(&self, plugin: P, config: P::Config) -> Result<FiberHandle> {
        self.plugins.spawn(plugin, config).await
    }

    pub async fn restart(&self, fiber: &FiberHandle) -> Result<()> {
        self.plugins.restart(fiber).await
    }

    pub async fn stop(&self) -> Result<()> {
        self.plugins.stop().await
    }

    pub fn is_shutdown(&self) -> bool {
        self.lifecycle.is_shutdown()
    }
}
