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
use parking_lot::{RwLock, RwLockWriteGuard};
use shenko_types::generation::GenerationRef;

use crate::capability::BackendCapabilitySnapshot;
use crate::profile::ProviderProfile;
use crate::request::{InferenceRequest, InferenceResponse, ProviderBinding};
use crate::stream::{BoundedStreamPublisher, StreamOutcome};

/// The Cordis service name of the provider registry.
pub const PROVIDER_REGISTRY_SERVICE: &str = "shenko/provider-registry";

/// The prepared provider runtimes of one publication generation.
///
/// This is the single authoritative provider read path once a generation is
/// attached. Preparation builds the real runtimes outside any Node transaction;
/// publication is one in-memory pointer swap; this port only resolves what that
/// generation owns.
///
/// `runtime_for` must resolve by exact binding identity, including the prepared
/// implementation generation the request was sealed against. A runtime built
/// from a displaced generation is still owned by that generation and remains
/// resolvable while a pinned step or in-flight reader retains it; a replacement
/// runtime never stands in for it.
pub trait PreparedProviderSource: Send + Sync + 'static {
    /// The generation this source publishes, when one is published.
    fn generation_ref(&self) -> Option<GenerationRef>;
    /// The runtime that executes exactly this sealed binding.
    fn runtime_for(&self, binding: &ProviderBinding) -> Option<Arc<dyn ProviderRuntime>>;
    /// The bindings of the current generation.
    fn current_bindings(&self) -> Vec<ProviderBinding>;
    /// The profiles of the current generation.
    fn current_profiles(&self) -> Vec<ProviderProfile>;
}

/// A refused prepared-provider attachment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreparedSourceAttachError {
    #[error("a prepared provider source is already attached")]
    AlreadyAttached,
    #[error("no prepared generation is published")]
    NoGenerationPublished,
    #[error("prepared provider source for generation {incoming:?} replaces the attached generation {attached:?}")]
    DifferentGeneration {
        attached: Option<GenerationRef>,
        incoming: Option<GenerationRef>,
    },
}

/// One installed provider runtime.
///
/// A runtime owns its wire, its transport and its native extensions. It is
/// asked to complete, stream, count and probe, and declares which models its
/// transport accepts. Model selection and capability authorization remain
/// with the caller.
#[async_trait::async_trait]
pub trait ProviderRuntime: Send + Sync + 'static {
    /// The frozen, non-secret description of this binding.
    fn profile(&self) -> &ProviderProfile;
    /// The binding this runtime executes.
    fn binding(&self) -> ProviderBinding;
    /// Whether this runtime accepts the caller-selected model for fresh
    /// admission. Fixed-model runtimes accept only their configured model;
    /// transports that carry a caller-selected model override this method.
    /// This is a synchronous local declaration, never a network probe or a
    /// promise that the remote account can currently execute the model.
    fn accepts_model(&self, model: &str) -> bool {
        self.profile().model_id == model
    }
    /// Run one complete request.
    async fn complete(&self, request: &InferenceRequest) -> Result<InferenceResponse>;
    /// Run one request and publish its stream.
    async fn stream(
        &self,
        request: &InferenceRequest,
        publisher: &BoundedStreamPublisher,
    ) -> Result<StreamOutcome>;
    /// End only the local observation when supported by this implementation.
    /// The default retains and awaits the real worker. An implementation may
    /// drop an HTTP exchange here only when that owns all its local work; it
    /// must not detach a blocking guest or claim that the remote effect ended.
    async fn complete_cancellable(&self, request: &InferenceRequest,
        stop: &tokio_util::sync::CancellationToken) -> Result<InferenceResponse>
    {
        if stop.is_cancelled() { return Err(crate::execution::LocalObservationStopped.into()); }
        self.complete(request).await
    }

    async fn stream_cancellable(&self, request: &InferenceRequest,
        publisher: &BoundedStreamPublisher, stop: &tokio_util::sync::CancellationToken)
        -> Result<StreamOutcome>
    {
        if stop.is_cancelled() { return Err(crate::execution::LocalObservationStopped.into()); }
        self.stream(request, publisher).await
    }
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
    /// The prepared generation whose provider catalog is authoritative while it
    /// is attached. Compiled transport runtimes stay in `entries`; a prepared
    /// binding can only ever be resolved by this source.
    prepared: RwLock<Option<Arc<dyn PreparedProviderSource>>>,
    /// Retained allocation identity of this exact attachment target.
    prepared_target: Arc<()>,
}

/// A checked provider attachment holding the actual prepared-source lock.
///
/// A coordinator acquires the kernel runtime guard first, then this guard,
/// preflights both ports, and retains both guards through both pointer stores.
/// Existing compiled entries and their installation epochs are untouched.
pub struct PreparedProviderAttachment<'a> {
    prepared: RwLockWriteGuard<'a, Option<Arc<dyn PreparedProviderSource>>>,
    target: &'a Arc<()>,
    incoming: Option<Arc<dyn PreparedProviderSource>>,
}

impl PreparedProviderAttachment<'_> {
    /// Opaque allocation identity of the locked registry, with no authority.
    pub fn target(&self) -> Arc<()> {
        Arc::clone(self.target)
    }

    /// Infallible pointer store, retaining the lock until this guard is dropped.
    pub fn commit(&mut self) {
        if self.prepared.is_none() {
            *self.prepared = self.incoming.take();
        }
    }
}

impl Service for ProviderRegistry {
    const NAME: &'static str = PROVIDER_REGISTRY_SERVICE;
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the prepared generation's provider catalog.
    ///
    /// Single-shot: a second attachment is refused, so a live process cannot
    /// hold two authoritative provider generations. A successor is published by
    /// the generation owner's one swap, which reaches this source, not this
    /// method.
    pub fn attach_prepared_source(
        &self,
        source: Arc<dyn PreparedProviderSource>,
    ) -> Result<(), PreparedSourceAttachError> {
        self.prepare_prepared_source_attachment(source)?.commit();
        Ok(())
    }

    /// Check exact source identity under the live attachment lock, without
    /// changing it. The same Arc is idempotent; identical generation metadata
    /// from another source cannot replace the installed port.
    pub fn prepare_prepared_source_attachment(
        &self,
        source: Arc<dyn PreparedProviderSource>,
    ) -> Result<PreparedProviderAttachment<'_>, PreparedSourceAttachError> {
        let prepared = self.prepared.write();
        let incoming = source.generation_ref();
        if incoming.is_none() {
            return Err(PreparedSourceAttachError::NoGenerationPublished);
        }
        if let Some(attached) = prepared.as_ref() {
            if !Arc::ptr_eq(attached, &source) {
                let attached_ref = attached.generation_ref();
                if attached_ref == incoming {
                    return Err(PreparedSourceAttachError::AlreadyAttached);
                }
                return Err(PreparedSourceAttachError::DifferentGeneration {
                    attached: attached_ref,
                    incoming,
                });
            }
        }
        Ok(PreparedProviderAttachment {
            prepared,
            target: &self.prepared_target,
            incoming: Some(source),
        })
    }

    /// The prepared generation whose provider catalog is authoritative.
    pub fn prepared_generation(&self) -> Option<GenerationRef> {
        self.prepared
            .read()
            .as_ref()
            .and_then(|source| source.generation_ref())
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

    /// Snapshot the installed compiled transport runtimes for generation preparation.
    ///
    /// Reads only the compiled entries under their registry lock. An attached
    /// prepared source may shadow these entries during ordinary discovery, but
    /// its runtimes must never be recaptured with compiled role bindings.
    /// Runtime identity and installation order are preserved by cloning the Arcs;
    /// the returned snapshot does not retain the registry lock.
    pub fn compiled_runtimes(&self) -> Vec<Arc<dyn ProviderRuntime>> {
        self.entries
            .read()
            .iter()
            .map(|entry| Arc::clone(&entry.runtime))
            .collect()
    }

    /// The runtime of one profile id.
    pub fn get(&self, profile_id: &str) -> Option<Arc<dyn ProviderRuntime>> {
        let prepared = self.prepared.read().as_ref().map(Arc::clone);
        if let Some(source) = prepared {
            if let Some(binding) = source
                .current_bindings()
                .into_iter()
                .find(|binding| binding.profile_id() == profile_id)
            {
                return source.runtime_for(&binding)
                    .filter(|runtime| runtime.binding().same_transport(&binding));
            }
        }
        self.entries
            .read()
            .iter()
            .find(|entry| entry.runtime.profile().profile_id == profile_id)
            .map(|entry| Arc::clone(&entry.runtime))
    }

    /// Every installed profile.
    pub fn profiles(&self) -> Vec<ProviderProfile> {
        let mut profiles = self
            .prepared
            .read()
            .as_ref()
            .map(|source| source.current_profiles())
            .unwrap_or_default();
        for entry in self.entries.read().iter() {
            let profile = entry.runtime.profile();
            if !profiles
                .iter()
                .any(|known| known.profile_id == profile.profile_id)
            {
                profiles.push(profile.clone());
            }
        }
        profiles
    }

    /// The runtime that can execute one sealed binding.
    ///
    /// The sealed binding is authoritative: a runtime whose own profile
    /// changed since the request was sealed cannot serve it, and a request
    /// whose model was never installed is not silently routed to another
    /// provider.
    ///
    /// The comparison covers every profile field except the model, so the
    /// installed process generation of a Code provider participates: an
    /// installation that replaced the executable behind identical transport
    /// metadata cannot serve a request sealed against the earlier bytes.
    pub fn runtime_for(&self, binding: &ProviderBinding) -> Option<Arc<dyn ProviderRuntime>> {
        // The prepared generation answers first. A prepared binding can only be
        // served by the exact generation it was sealed against; the compiled
        // entries below cannot match it because the binding comparison includes
        // that prepared identity.
        if let Some(source) = self.prepared.read().as_ref() {
            if let Some(runtime) = source.runtime_for(binding) {
                if runtime.binding().same_transport(binding) {
                    return Some(runtime);
                }
            }
        }
        if binding.prepared().is_some() {
            return None;
        }
        let entries = self.entries.read();
        match binding {
            ProviderBinding::Profile { profile, prepared } => entries
                .iter()
                .find(|entry| {
                    prepared.is_none() && entry.runtime.profile().matches_except_model(profile)
                        && entry.runtime.binding().prepared().is_none()
                })
                .map(|entry| Arc::clone(&entry.runtime)),
            other => entries
                .iter()
                .find(|entry| entry.runtime.binding().same_transport(other))
                .map(|entry| Arc::clone(&entry.runtime)),
        }
    }

    /// Fresh eligibility uses the current prepared declaration first, then the
    /// installed compiled profile. It never authorizes a displaced generation
    /// or a model refused by its runtime, and never gates original-request reconciliation.
    pub(crate) fn serves_model(&self, binding: &ProviderBinding, model: &str) -> bool {
        let prepared = self.prepared.read();
        if let Some(source) = prepared.as_ref() {
            if let Some(current) = source.current_bindings().into_iter()
                .find(|current| current.profile_id() == binding.profile_id())
            {
                if !current.same_transport(binding) {
                    return false;
                }
                return source.runtime_for(&current).is_some_and(|runtime|
                    runtime.accepts_model(model) && runtime.available()
                        && runtime.binding().same_transport(&current));
            }
        }
        if binding.prepared().is_some() {
            return false;
        }
        let entries = self.entries.read();
        let Some(entry) = entries.iter().find(|entry|
            entry.runtime.profile().profile_id == binding.profile_id()) else {
            return false;
        };
        let runtime = &entry.runtime;
        let profile = runtime.profile();
        runtime.accepts_model(model) && runtime.available() && match binding {
            ProviderBinding::Profile { profile: bound, prepared } => {
                prepared.is_none() && profile.matches_except_model(bound)
                    && runtime.binding().prepared().is_none()
            }
            ProviderBinding::Stub { .. } => false,
            other => runtime.binding().same_transport(other),
        }
    }

    /// The binding of the first installed runtime. It is the binding a fresh
    /// broker seals with; the runtime itself stays authoritative.
    pub fn primary_binding(&self) -> Option<ProviderBinding> {
        if let Some(binding) = self
            .prepared
            .read()
            .as_ref()
            .and_then(|source| source.current_bindings().into_iter().next())
        {
            return Some(binding);
        }
        self.entries.read().first().map(|entry| entry.runtime.binding())
    }

    /// The binding of the runtime that owns one profile.
    pub fn primary_binding_for(&self, profile: &ProviderProfile) -> Option<ProviderBinding> {
        if let Some(binding) = self.prepared.read().as_ref().and_then(|source| {
            source
                .current_bindings()
                .into_iter()
                .find(|binding| binding.profile_id() == profile.profile_id)
        }) {
            return Some(binding);
        }
        self.entries
            .read()
            .iter()
            .find(|entry| entry.runtime.profile().profile_id == profile.profile_id)
            .map(|entry| entry.runtime.binding())
    }

    /// The provider/model pairs this broker can really run. Availability is
    /// decided by the installation, never by a catalog of model names.
    pub fn installed_bindings(&self) -> Vec<ProviderBinding> {
        let mut bindings = self
            .prepared
            .read()
            .as_ref()
            .map(|source| source.current_bindings())
            .unwrap_or_default();
        for entry in self.entries.read().iter() {
            let binding = entry.runtime.binding();
            if !bindings
                .iter()
                .any(|known| known.profile_id() == binding.profile_id())
            {
                bindings.push(binding);
            }
        }
        bindings
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::native_backends;
    use crate::request::{InferenceRequest, InferenceResponse};
    use crate::stream::{BoundedStreamPublisher, StreamOutcome};
    use shenko_types::generation::{PreparedRole, SealedGenerationBinding};
    use shenko_types::object::{ArtifactRef, Digest, SchemaRef, VersionedRef};

    // Retained publication fixture: identity and local declarations only.
    // No operation fabricates a wire result or an availability observation.
    struct FixtureRuntime {
        binding: ProviderBinding,
        accepts_selected: bool,
        available: bool,
    }

    #[async_trait::async_trait]
    impl ProviderRuntime for FixtureRuntime {
        fn profile(&self) -> &ProviderProfile { self.binding.profile().unwrap() }
        fn binding(&self) -> ProviderBinding { self.binding.clone() }
        fn accepts_model(&self, model: &str) -> bool {
            self.accepts_selected || self.profile().model_id == model
        }
        fn available(&self) -> bool { self.available }
        async fn complete(&self, _: &InferenceRequest) -> Result<InferenceResponse> {
            anyhow::bail!("fixture provider runtime has no wire")
        }
        async fn stream(&self, _: &InferenceRequest, _: &BoundedStreamPublisher) -> Result<StreamOutcome> {
            anyhow::bail!("fixture provider runtime has no wire")
        }
        async fn count_tokens(&self, _: &InferenceRequest) -> Result<TokenCount> {
            anyhow::bail!("fixture provider runtime has no wire")
        }
        async fn probe(&self) -> Result<BackendCapabilitySnapshot> {
            anyhow::bail!("fixture provider runtime has no wire")
        }
    }

    struct FixtureSource {
        generation: Option<GenerationRef>,
        binding: ProviderBinding,
        runtime: Option<Arc<dyn ProviderRuntime>>,
    }

    impl PreparedProviderSource for FixtureSource {
        fn generation_ref(&self) -> Option<GenerationRef> { self.generation.clone() }
        fn runtime_for(&self, binding: &ProviderBinding) -> Option<Arc<dyn ProviderRuntime>> {
            self.binding.same_transport(binding).then(|| self.runtime.clone()).flatten()
        }
        fn current_bindings(&self) -> Vec<ProviderBinding> { vec![self.binding.clone()] }
        fn current_profiles(&self) -> Vec<ProviderProfile> { vec![self.binding.profile().unwrap().clone()] }
    }

    fn binding(artifact: &[u8]) -> ProviderBinding {
        let profile = native_backends::vllm(
            "http://127.0.0.1:11434", "fixture-model", "fixture-domain",
        );
        let digest = Digest::blake3(artifact);
        ProviderBinding::Profile {
            profile,
            prepared: Some(SealedGenerationBinding::compiled(
                PreparedRole::Provider,
                VersionedRef {
                    artifact: ArtifactRef { digest: digest.clone(), locator: "native:fixture".into() },
                    symbol: "fixture.provider".into(),
                    schema: SchemaRef { name: "fixture/provider".into(), major: 1, digest },
                },
                Digest::blake3(b"descriptor"),
                Digest::blake3(b"closure"),
            )),
        }
    }

    fn runtime(binding: ProviderBinding, accepts_selected: bool, available: bool) -> Arc<dyn ProviderRuntime> {
        Arc::new(FixtureRuntime { binding, accepts_selected, available })
    }

    fn source(binding: ProviderBinding, runtime: Option<Arc<dyn ProviderRuntime>>) -> Arc<dyn PreparedProviderSource> {
        Arc::new(FixtureSource {
            generation: Some(GenerationRef::new(1, Digest::blake3(b"catalog"))), binding, runtime,
        })
    }

    #[test]
    fn service_attachments_are_single_shot_and_require_a_published_generation() {
        let registry = ProviderRegistry::new();
        let binding = binding(b"original");
        let unpublished = Arc::new(FixtureSource { generation: None, binding: binding.clone(), runtime: None });
        assert_eq!(registry.attach_prepared_source(unpublished), Err(PreparedSourceAttachError::NoGenerationPublished));
        let first = source(binding.clone(), None);
        registry.attach_prepared_source(Arc::clone(&first)).unwrap();
        registry.attach_prepared_source(first).unwrap();
        assert_eq!(registry.attach_prepared_source(source(binding, None)), Err(PreparedSourceAttachError::AlreadyAttached));
    }

    #[test]
    fn compiled_provider_inventory_excludes_published_roles_and_preserves_shadowed_entries() {
        let registry = ProviderRegistry::new();
        let prepared = binding(b"original");
        let compiled = runtime(prepared.profile().unwrap().binding(), false, true);
        let epoch = registry.install(Arc::clone(&compiled));
        let published = runtime(prepared.clone(), false, true);
        registry.attach_prepared_source(source(prepared.clone(), Some(Arc::clone(&published)))).unwrap();
        assert!(Arc::ptr_eq(&registry.compiled_runtimes()[0], &compiled));
        assert!(Arc::ptr_eq(&registry.get(prepared.profile_id()).unwrap(), &published));
        assert_eq!(registry.installed_bindings(), vec![prepared.clone()]);
        assert_eq!(registry.profiles(), vec![prepared.profile().unwrap().clone()]);
        assert!(registry.uninstall(prepared.profile_id(), epoch));
        assert!(registry.compiled_runtimes().is_empty());
        assert!(registry.runtime_for(&prepared).is_some());
    }

    #[test]
    fn prepared_bindings_resolve_only_through_the_prepared_source() {
        let registry = ProviderRegistry::new();
        let original = binding(b"original");
        // Even a compiled entry with copied prepared metadata grants no authority.
        registry.install(runtime(original.clone(), true, true));
        assert!(registry.runtime_for(&original).is_none());
        assert!(!registry.serves_model(&original, "selected-model"));
        let published = runtime(original.clone(), true, true);
        registry.attach_prepared_source(source(original.clone(), Some(Arc::clone(&published)))).unwrap();
        assert!(Arc::ptr_eq(&registry.runtime_for(&original).unwrap(), &published));
        let replacement = binding(b"replacement");
        assert_eq!(original.profile(), replacement.profile());
        assert!(registry.runtime_for(&replacement).is_none());
        assert!(!registry.serves_model(&replacement, "selected-model"));
    }

    #[test]
    fn fresh_eligibility_preserves_runtime_model_ownership_and_availability() {
        let original = binding(b"original");
        for prepared in [false, true] {
            for accepts_selected in [false, true] {
                for available in [false, true] {
                    let registry = ProviderRegistry::new();
                    let binding = if prepared { original.clone() } else { original.profile().unwrap().binding() };
                    let provider = runtime(binding.clone(), accepts_selected, available);
                    if prepared {
                        registry.attach_prepared_source(source(binding.clone(), Some(provider))).unwrap();
                        // A live compiled shadow cannot make an unavailable or
                        // fixed-model prepared role eligible.
                        registry.install(runtime(original.profile().unwrap().binding(), true, true));
                    } else { registry.install(provider); }
                    assert_eq!(registry.serves_model(&binding, "selected-model"), accepts_selected && available);
                    assert_eq!(registry.serves_model(&binding, binding.model()), available);
                    // Recovery lookup is independent of fresh eligibility.
                    assert!(registry.runtime_for(&binding).is_some());
                }
            }
        }
    }

    #[test]
    fn missing_or_foreign_prepared_runtime_never_uses_a_compiled_shadow() {
        let original = binding(b"original");
        for prepared_runtime in [None, Some(runtime(binding(b"replacement"), true, true))] {
            let registry = ProviderRegistry::new();
            let historical = original.profile().unwrap().binding();
            registry.install(runtime(historical.clone(), true, true));
            registry.attach_prepared_source(source(original.clone(), prepared_runtime)).unwrap();
            assert!(registry.get(original.profile_id()).is_none());
            assert!(registry.runtime_for(&original).is_none());
            assert!(!registry.serves_model(&original, "selected-model"));
            // Historical sealed transport lookup retains its original owner.
            assert!(registry.runtime_for(&historical).is_some());
            assert!(!registry.serves_model(&historical, "selected-model"));
        }
    }

    #[test]
    fn historical_profile_binding_omits_generation_fields_and_round_trips() {
        let original = binding(b"original");
        let historical = original.profile().unwrap().binding();
        let value = serde_json::to_value(&historical).unwrap();
        assert!(value.get("prepared").is_none());
        assert!(value["profile"].get("process_generation").is_none());
        let decoded: ProviderBinding = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded, historical);
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        assert!(!historical.same_transport(&original));
        let selected = original.with_model("selected-model").unwrap();
        assert_eq!(selected.prepared(), original.prepared());
        assert!(selected.same_transport(&original));
        assert_eq!(selected.model(), "selected-model");
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
