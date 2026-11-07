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
use serde::{Deserialize, Serialize};

use crate::hooks::{GenerationRetirement, HookPublication};
use crate::lifecycle::LifecycleState;
use crate::errors::{ErrorObservation, ErrorSink};
use crate::plugin::PluginRegistry;
use crate::runtime::{GenerationRegistrations, PluginRuntime, RegistrationWithdrawal};
use crate::services::RuntimeServicesPlugin;
use shenko_types::ids::{EntityId, RuntimeGeneration};

/// The tokens one installed plugin generation owns.
///
/// The caller assembles this from what its install path actually created: the
/// Cordis Fiber, the hook publication, and the live-registry handles. It is
/// not a second registry, and the supervisor does not discover members.
pub struct InstalledPlugin {
    pub plugin_id: EntityId,
    pub fiber: FiberHandle,
    /// The hook generation this plugin published, when it publishes handlers.
    pub hooks: Option<HookPublication>,
    /// The provider/capability/help registrations this generation owns.
    pub registrations: GenerationRegistrations,
}

/// What an uninstall actually retired, in the order it retired it.
///
/// A receipt is returned only when the whole order completed. A failure
/// reports how far it got instead of claiming either full cleanup or none.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UninstallReceipt {
    pub plugin_id: EntityId,
    /// The hook withdrawal and its real dispatch barrier, when the plugin
    /// published handlers.
    pub hooks: Option<GenerationRetirement>,
    pub registrations: RegistrationWithdrawal,
    /// True only after Cordis's own generation cleanup returned.
    pub plugin_disposed: bool,
}

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

    /// Publish one plugin's hook generation as a single graph revision.
    ///
    /// `HookRegistry::validate_generation` runs the same candidate admission
    /// without publishing, so an install can refuse a bad graph before it
    /// creates a Fiber. The returned publication owns exactly the handlers
    /// this call added; hand it, the Fiber and the live-registry handles to
    /// [`Supervisor::uninstall_installed`] to retire the generation as one
    /// unit.
    pub fn install_generation(
        &self,
        plugin_id: EntityId,
        entries: Vec<crate::hooks::HookEntry>,
    ) -> Result<crate::hooks::HookPublication> {
        anyhow::ensure!(
            !entries.is_empty(),
            "an install generation must publish at least one handler"
        );
        anyhow::ensure!(
            entries
                .iter()
                .all(|entry| entry.spec.plugin_id == plugin_id),
            "a plugin generation may only publish handlers of its own plugin identity"
        );
        Ok(self.hooks.install_generation(entries)?)
    }

    /// Replace one plugin's handlers with a new generation and report the
    /// graph revision the old ones were admitted under.
    ///
    /// The replacement is one atomic publication; the retired revision is the
    /// barrier the caller awaits (`Supervisor::retire_generation`) before
    /// releasing resources the old handlers still use.
    pub fn upgrade_generation(
        &self,
        plugin_id: EntityId,
        entries: Vec<crate::hooks::HookEntry>,
    ) -> Result<crate::hooks::GenerationSwap> {
        Ok(self.hooks.upgrade_generation(plugin_id, entries)?)
    }

    /// Withdraw a published hook generation and wait for its dispatch barrier.
    pub async fn retire_generation(
        &self,
        publication: &crate::hooks::HookPublication,
    ) -> Result<GenerationRetirement> {
        Ok(self.hooks.retire_generation(publication).await?)
    }

    /// Retire one whole installed plugin generation in the order the
    /// documented contract requires.
    ///
    /// 1. The hook generation leaves new-request discovery and the dispatch
    ///    barrier of this process is awaited, so no dispatch is still running
    ///    on resources the plugin is about to lose. Do not call this from
    ///    inside a hook handler or any other live dispatch: that dispatch owns
    ///    the lease this waits for.
    /// 2. The live-registry registrations this generation owns are withdrawn
    ///    with their own epochs; a successor's replacement is left in place.
    /// 3. Cordis disposes the Fiber and awaits the plugin's own generation
    ///    cleanup: services withdrawn, workers joined, timers dropped.
    ///
    /// A refused hook withdrawal (it would break another handler's declared
    /// hard dependency) stops the uninstall before steps 2 and 3, so the
    /// plugin and its registrations stay consistent.
    pub async fn uninstall_installed(
        &self,
        plugin_id: EntityId,
        mut installed: InstalledPlugin,
    ) -> Result<UninstallReceipt> {
        anyhow::ensure!(
            installed.plugin_id == plugin_id,
            "uninstall receipt names a different plugin generation"
        );
        let hooks = match installed.hooks.as_ref() {
            Some(publication) => Some(
                self.hooks
                    .retire_generation(publication)
                    .await
                    .map_err(|error| anyhow::anyhow!("hook generation withdrawal refused: {error}"))?,
            ),
            None => None,
        };
        let registrations = installed.registrations.withdraw(&self.runtime);
        self.plugins
            .uninstall(&installed.fiber)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "plugin cleanup failed after hooks and registrations were retired: {error}"
                )
            })?;
        Ok(UninstallReceipt {
            plugin_id,
            hooks,
            registrations,
            plugin_disposed: true,
        })
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
