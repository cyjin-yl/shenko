//! Minimal supervisor — starts plugins, holds lifecycle state.
use std::sync::Arc;
use anyhow::Result;
use tracing::info;

use shenko_types::ids::RuntimeGeneration;
use crate::lifecycle::LifecycleState;
use crate::plugin::PluginRegistry;
use crate::capability::CapabilityRegistry;
use crate::service::ServiceRegistry;

/// The Shenko supervisor. Does NOT depend on any plugin implementation.
pub struct Supervisor {
    pub lifecycle: Arc<LifecycleState>,
    pub plugins: Arc<PluginRegistry>,
    pub capabilities: Arc<CapabilityRegistry>,
    pub services: Arc<ServiceRegistry>,
}

impl Supervisor {
    pub fn new(generation: RuntimeGeneration) -> Self {
        Self {
            lifecycle: Arc::new(LifecycleState::new(generation)),
            plugins: Arc::new(PluginRegistry::new()),
            capabilities: Arc::new(CapabilityRegistry::new()),
            services: Arc::new(ServiceRegistry::new()),
        }
    }

    pub async fn start(&self) -> Result<()> {
        info!(generation = %self.lifecycle.generation, "supervisor starting");
        self.plugins.start_all().await?;
        info!("supervisor started");
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        info!("supervisor stopping");
        self.lifecycle.shutdown();
        self.plugins.stop_all().await?;
        info!("supervisor stopped");
        Ok(())
    }

    pub fn is_shutdown(&self) -> bool {
        self.lifecycle.is_shutdown()
    }
}
