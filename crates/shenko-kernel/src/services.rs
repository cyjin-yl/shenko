//! The runtime's own services — docs/specs/plugins.md#spec-7, docs/specs/context-policies.md#spec-37.
//!
//! The assembly root owns two live registries: the hook graph and the plugin
//! runtime registry. Both are published on the runtime's root context as
//! ordinary Cordis services, so a plugin resolves them through the native
//! service lookup instead of keeping its own copy or reaching for a global.
//!
//! They are published together by one plugin: a plugin that can see the hook
//! graph can also see what the runtime currently offers, and both disappear
//! together when the root is torn down.
use std::convert::Infallible;
use std::sync::Arc;

use cordis_core::{Context, Plugin, Service};

use crate::hooks::HookRegistry;
use crate::runtime::PluginRuntime;

/// The service name of the hook graph.
pub const HOOK_REGISTRY_SERVICE: &str = "shenko.hooks/v1";

impl Service for HookRegistry {
    const NAME: &'static str = HOOK_REGISTRY_SERVICE;
}

/// Publishes the hook graph and the live plugin runtime registry.
pub struct RuntimeServicesPlugin;

impl Plugin for RuntimeServicesPlugin {
    type Config = (Arc<PluginRuntime>, Arc<HookRegistry>);
    type Input = (Arc<PluginRuntime>, Arc<HookRegistry>);
    type PrepareError = Infallible;
    type ApplyError = cordis_core::service::ServicePublishError;

    fn name(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("shenko-runtime-services")
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, services: &Self::Input) -> Result<(), Self::ApplyError> {
        let _runtime = ctx.provide(services.0.clone())?;
        let _hooks = ctx.provide(services.1.clone())?;
        // The registry and the graph are published together, so they are
        // bound together: a capability that appears or disappears produces a
        // `runtime.capability_changed` event in the graph.
        services.0.attach_hook_graph(services.1.clone());
        Ok(())
    }
}
