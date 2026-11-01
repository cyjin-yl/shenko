//! The provider runtime registry — open runtime provider assembly.
//!
//! A provider is an installable runtime implementation, not a closed enum. Any
//! plugin may register a runtime; the registry is the read model over the
//! installed runtimes and holds no durable state of its own. Installation
//! facts stay with the installation authority; this registry only mirrors what
//! is currently live, and a withdrawn plugin removes exactly its own entry.
//!
//! The registry is published as a Cordis service, so the composition root and
//! every plugin resolve the same instance.

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::Result;
use cordis_core::{Context, InjectSpec, Plugin, Service};
use parking_lot::RwLock;

use crate::capability::BackendCapabilitySnapshot;
use crate::profile::ProviderProfile;
use crate::request::{InferenceRequest, InferenceResponse, ProviderBinding};
use crate::stream::{BoundedStreamPublisher, StreamOutcome};

/// The Cordis service name of the provider registry.
pub const PROVIDER_REGISTRY_SERVICE: &str = "shenko/provider-registry";

/// One installed provider runtime.
///
/// A runtime owns its wire, its transport and its native extensions. It is
/// asked to complete, stream, count and probe; it never decides which model
/// may be used or whether a capability is allowed.
#[async_trait::async_trait]
pub trait ProviderRuntime: Send + Sync + 'static {
    /// The frozen, non-secret description of this binding.
    fn profile(&self) -> &ProviderProfile;
    /// The binding this runtime executes.
    fn binding(&self) -> ProviderBinding;
    /// Run one complete request.
    async fn complete(&self, request: &InferenceRequest) -> Result<InferenceResponse>;
    /// Run one request and publish its stream.
    async fn stream(
        &self,
        request: &InferenceRequest,
        publisher: &BoundedStreamPublisher,
    ) -> Result<StreamOutcome>;
    /// Count the tokens one request would send, when the binding supports it.
    async fn count_tokens(&self, request: &InferenceRequest) -> Result<TokenCount>;
    /// Observe what this backend actually offers.
    async fn probe(&self) -> Result<BackendCapabilitySnapshot>;
    /// Whether this runtime can serve requests at all. An offline binding
    /// reports `false` and the broker answers with a typed unavailable
    /// outcome instead of a fabricated result.
    fn available(&self) -> bool {
        true
    }
}

/// The token count a binding reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenCount {
    pub input_tokens: u64,
    /// The binding that produced the count.
    pub binding: ProviderBinding,
}

/// One installed runtime and its installation epoch.
struct InstalledProvider {
    runtime: Arc<dyn ProviderRuntime>,
    epoch: u64,
}

/// The live registry of provider runtimes.
#[derive(Default)]
pub struct ProviderRegistry {
    entries: RwLock<Vec<InstalledProvider>>,
    next_epoch: RwLock<u64>,
}

impl Service for ProviderRegistry {
    const NAME: &'static str = PROVIDER_REGISTRY_SERVICE;
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install one runtime, replacing a previous runtime with the same profile
    /// id. The returned epoch identifies this installation: a late cleanup of
    /// an older generation cannot uninstall its successor.
    pub fn install(&self, runtime: Arc<dyn ProviderRuntime>) -> u64 {
        let mut next = self.next_epoch.write();
        *next += 1;
        let epoch = *next;
        drop(next);
        let profile_id = runtime.profile().profile_id.clone();
        let mut entries = self.entries.write();
        entries.retain(|entry| entry.runtime.profile().profile_id != profile_id);
        entries.push(InstalledProvider { runtime, epoch });
        epoch
    }

    /// Remove exactly one installation.
    pub fn uninstall(&self, profile_id: &str, epoch: u64) -> bool {
        let mut entries = self.entries.write();
        let before = entries.len();
        entries.retain(|entry| {
            !(entry.runtime.profile().profile_id == profile_id && entry.epoch == epoch)
        });
        entries.len() != before
    }

    /// The runtime of one profile id.
    pub fn get(&self, profile_id: &str) -> Option<Arc<dyn ProviderRuntime>> {
        self.entries
            .read()
            .iter()
            .find(|entry| entry.runtime.profile().profile_id == profile_id)
            .map(|entry| Arc::clone(&entry.runtime))
    }

    /// Every installed profile.
    pub fn profiles(&self) -> Vec<ProviderProfile> {
        self.entries
            .read()
            .iter()
            .map(|entry| entry.runtime.profile().clone())
            .collect()
    }

    /// The runtime that can execute one sealed binding.
    ///
    /// The sealed binding is authoritative: a runtime whose own profile
    /// changed since the request was sealed cannot serve it, and a request
    /// whose model was never installed is not silently routed to another
    /// provider.
    pub fn runtime_for(&self, binding: &ProviderBinding) -> Option<Arc<dyn ProviderRuntime>> {
        let entries = self.entries.read();
        match binding {
            ProviderBinding::Profile { profile } => entries
                .iter()
                .find(|entry| entry.runtime.profile().matches_except_model(profile))
                .map(|entry| Arc::clone(&entry.runtime)),
            other => entries
                .iter()
                .find(|entry| entry.runtime.binding().same_transport(other))
                .map(|entry| Arc::clone(&entry.runtime)),
        }
    }

    /// The binding of the first installed runtime. It is the binding a fresh
    /// broker seals with; the runtime itself stays authoritative.
    pub fn primary_binding(&self) -> Option<ProviderBinding> {
        self.entries.read().first().map(|entry| entry.runtime.binding())
    }

    /// The binding of the runtime that owns one profile.
    pub fn primary_binding_for(&self, profile: &ProviderProfile) -> Option<ProviderBinding> {
        self.entries
            .read()
            .iter()
            .find(|entry| entry.runtime.profile().profile_id == profile.profile_id)
            .map(|entry| entry.runtime.binding())
    }

    /// The provider/model pairs this broker can really run. Availability is
    /// decided by the installation, never by a catalog of model names.
    pub fn installed_bindings(&self) -> Vec<ProviderBinding> {
        self.entries.read().iter().map(|entry| entry.runtime.binding()).collect()
    }
}

/// Publishes a caller-owned provider registry for the lifetime of this Fiber
/// generation. The registry outlives plugin restarts; providers require it by
/// name.
pub struct ProviderRegistryHost;

impl Plugin for ProviderRegistryHost {
    type Config = Arc<ProviderRegistry>;
    type Input = Arc<ProviderRegistry>;
    type PrepareError = std::convert::Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("provider-registry-host")
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let _publication = ctx.provide(input.clone()).map_err(std::io::Error::other)?;
        Ok(())
    }
}

/// Installs one provider runtime for the lifetime of this Fiber generation.
///
/// A restart or an update installs a new epoch; the previous installation is
/// removed by its own cleanup, so a late dispose cannot uninstall the
/// successor.
pub struct ProviderInstaller {
    runtime: Arc<dyn ProviderRuntime>,
}

impl ProviderInstaller {
    pub fn new(runtime: Arc<dyn ProviderRuntime>) -> Self {
        Self { runtime }
    }
}

impl Plugin for ProviderInstaller {
    type Config = Arc<dyn ProviderRuntime>;
    type Input = Arc<dyn ProviderRuntime>;
    type PrepareError = std::io::Error;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("provider-installer")
    }

    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(ProviderRegistry::NAME)
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        config.profile().validate().map_err(std::io::Error::other)?;
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let registry = ctx
            .try_service::<ProviderRegistry>()
            .map_err(std::io::Error::other)?;
        let profile_id = input.profile().profile_id.clone();
        let epoch = registry.install(Arc::clone(input));
        ctx.effect_sync(move || {
            registry.uninstall(&profile_id, epoch);
        })
        .map_err(std::io::Error::other)?;
        Ok(())
    }
}
