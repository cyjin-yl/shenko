//! Minimal host policy around the native Cordis runtime, not a plugin framework.
//!
//! The supervisor owns the runtime root, the live plugin registry and the
//! generation's lifecycle state. It does not own the business services: those
//! are native Cordis plugins that publish their own services, and the live
//! registry they register with is resolved through the same root context.
use std::sync::Arc;

use anyhow::Result;
use cordis_core::lifecycle::UpdateOutcome;
use cordis_core::{Context, FiberHandle, Plugin};

use crate::lifecycle::LifecycleState;
use crate::errors::{ErrorObservation, ErrorSink};
use crate::plugin::PluginRegistry;
use crate::runtime::PluginRuntime;
use crate::services::RuntimeServicesPlugin;
use shenko_types::ids::RuntimeGeneration;

/// Business services are native Plugins with generation-owned Context bindings.
/// The binary's default business graph is installed through this API.
pub struct Supervisor {
    pub lifecycle: Arc<LifecycleState>,
    pub plugins: Arc<PluginRegistry>,
    /// The live provider/capability/help registry. It is published on the
    /// runtime root context, so a plugin resolves it through the Cordis
    /// service lookup rather than through a second locator.
    pub runtime: Arc<PluginRuntime>,
    /// The live hook graph. It is published on the runtime root context, so a
    /// plugin registers its handlers through the native service lookup.
    pub hooks: Arc<crate::hooks::HookRegistry>,
    /// The single error-observation point shared by the plugin registry and
    /// the hook registry: one kernel incident, one journal entry. It is
    /// counter+stderr until the assembly attaches the store-backed sink
    /// after genesis (the kernel must not manufacture a branch).
    pub errors: Arc<ErrorObservation>,
}

impl Supervisor {
    pub fn new(generation: RuntimeGeneration) -> Self {
        let lifecycle = Arc::new(LifecycleState::new(generation));
        let errors = Arc::new(ErrorObservation::new());
        Self {
            plugins: Arc::new(PluginRegistry::new_with_errors(
                lifecycle.shutdown_token(),
                Arc::clone(&errors),
            )),
            lifecycle,
            runtime: Arc::new(PluginRuntime::new()),
            hooks: Arc::new(crate::hooks::HookRegistry::new_with_errors(Arc::clone(
                &errors,
            ))),
            errors,
        }
    }

    /// Attach the store-backed error sink once the subject store exists.
    /// Before this call kernel failures are counted and traced, never
    /// journaled — at startup there is no branch to journal into.
    pub fn attach_error_sink(&self, sink: Arc<dyn ErrorSink>) {
        self.errors.attach(sink);
    }

    pub fn context(&self) -> &Context {
        self.plugins.context()
    }

    pub async fn spawn<P: Plugin>(&self, plugin: P, config: P::Config) -> Result<FiberHandle> {
        self.plugins.spawn(plugin, config).await
    }

    /// Publish the runtime's own services on the root context.
    ///
    /// After this call a plugin can resolve the live plugin registry and the
    /// hook graph through `Context::try_service`, and both are withdrawn when
    /// the root is torn down.
    pub async fn publish_services(&self) -> Result<FiberHandle> {
        self.plugins
            .spawn(
                RuntimeServicesPlugin,
                (self.runtime.clone(), self.hooks.clone()),
            )
            .await
    }

    pub async fn restart(&self, fiber: &FiberHandle) -> Result<()> {
        self.plugins.restart(fiber).await
    }

    /// Terminally end one installed plugin and await its real cleanup barrier.
    pub async fn uninstall(&self, fiber: &FiberHandle) -> Result<()> {
        self.plugins.uninstall(fiber).await
    }

    /// Replace the committed input of one installed plugin.
    pub async fn update<P: Plugin>(
        &self,
        fiber: &FiberHandle,
        input: P::Input,
    ) -> Result<UpdateOutcome> {
        self.plugins.update::<P>(fiber, input).await
    }

    pub async fn stop(&self) -> Result<()> {
        // The hook graph's in-flight dispatches hold a lease of the graph
        // they started with. The teardown waits for that real barrier
        // instead of assuming a dispatch already finished.
        self.hooks.await_current_quiescence().await;
        self.plugins.stop().await
    }

    pub fn is_shutdown(&self) -> bool {
        self.lifecycle.is_shutdown()
    }
}
