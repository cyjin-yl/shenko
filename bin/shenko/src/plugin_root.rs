//! The durable plugin-root lifecycle of an installed definition.
//!
//! The live plugin registry is a projection of this process; the durable
//! record is what survives a crash. The prepared, published and attached
//! phases are written by the composition that installs the definition; this
//! module owns the last phase: the retirement, recorded when the runtime
//! withdraws the definition.
//!
//! The retirement is its own plugin so it runs in the same teardown as the
//! live registration it retires. The plugin is spawned after the definition it
//! retires, so the definition's own withdrawal runs first and the retirement
//! is recorded after it.

use std::borrow::Cow;
use std::convert::Infallible;
use std::sync::Arc;

use cordis_core::{Context, InjectSpec, Plugin};
use shenko_store::Store;
use shenko_types::ids::{EntityId, RuntimeGeneration};
use shenko_types::plugin_runtime::PluginRetireRequest;

/// One installed definition's retirement.
pub struct PluginRootRetirement {
    /// The store that owns the durable lifecycle record.
    pub store: Arc<Store>,
    /// The installation this retirement belongs to.
    pub installation_id: EntityId,
    /// The process generation that withdrew the definition.
    pub runtime_generation: RuntimeGeneration,
    /// Why the runtime withdrew the definition.
    pub reason: String,
    /// Whether the Node installation still retains the implementation.
    pub artifact_retained: bool,
}

/// Records the retirement of one installed definition when the runtime
/// withdraws it.
pub struct PluginRootLifecyclePlugin;

impl Plugin for PluginRootLifecyclePlugin {
    type Config = PluginRootRetirement;
    type Input = PluginRootRetirement;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("shenko.plugin-root.lifecycle")
    }

    fn inject(&self) -> InjectSpec {
        InjectSpec::none()
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), Self::ApplyError> {
        let store = Arc::clone(&config.store);
        let installation_id = config.installation_id;
        let runtime_generation = config.runtime_generation.clone();
        let reason = config.reason.clone();
        let artifact_retained = config.artifact_retained;
        ctx.effect_sync(move || {
            store
                .retire_plugin_root(
                    installation_id,
                    &PluginRetireRequest {
                        runtime_generation,
                        reason,
                        artifact_retained,
                        evidence: Vec::new(),
                    },
                )
                .map(|_| ())
                // The cleanup boundary reports a returned error and takes a
                // standard error: the store's own error carries the detail.
                .map_err(std::io::Error::other)
        })
        .map_err(std::io::Error::other)?;
        Ok(())
    }
}
