//! Live plugin runtime registry — SPEC §18, §36, §37.10, §41.7.
//!
//! One registry owns the *current* view of what is installed: which
//! providers exist, which capabilities they offer, what each operation's
//! effect contract is, and which help document explains them. It is a Cordis
//! service, published on the runtime's root context, so a plugin registers
//! during its own `apply` and withdraws during its own teardown. There is no
//! startup snapshot and no second service locator: the view is produced from
//! the live registrations every time it is read.
//!
//! Every registration advances a monotonic revision. An install receipt, a
//! sealed request or a UI view can therefore name the exact registry state it
//! observed, and a later change is visible instead of silently reinterpreted.
//!
//! The registry is a *projection*, not a second source of truth. Durable facts
//! stay with their owners: the Node installation units keep what is installed
//! on a node, the Store keeps what actually happened, and this registry keeps
//! what this running process currently offers.

use std::collections::BTreeMap;
use std::sync::Arc;

use cordis_core::{Context, Plugin, PreparedPlugin};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use shenko_types::agent::AgentCapability;
use shenko_types::ids::EntityId;
use shenko_types::object::VersionedRef;
use shenko_types::plugin_runtime::{
    CapabilityView, HelpView, OperationEffectView, ProviderHealth, ProviderView, RuntimeView,
};

use crate::hooks::{HookPayload, HookRegistry};

/// The service name of the live runtime registry.
pub const PLUGIN_RUNTIME_SERVICE: &str = "shenko.runtime.plugin/v1";

/// A provider registration — one installed implementation family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRegistration {
    /// Stable identity of this provider, e.g. "native.repl.session".
    pub provider_id: String,
    /// Provider family, e.g. "native.task", "model.inference", "channel.qq".
    pub kind: String,
    /// The version of the implementation being registered.
    pub version: String,
    /// The plugin that owns this provider.
    pub plugin_id: EntityId,
    /// The exact implementation the registration describes.
    pub implementation: VersionedRef,
    /// One line describing what the provider does.
    pub description: String,
    /// The operations of this provider with their §36 effect contracts.
    pub operations: Vec<OperationEffectView>,
    /// The capability ids this provider offers.
    pub capabilities: Vec<String>,
    /// The observed health of this provider.
    pub health: ProviderHealth,
}

/// A capability registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityRegistration {
    pub capability_id: String,
    pub version: String,
    pub description: String,
    pub plugin_id: EntityId,
    /// The provider that offers it, when there is one.
    pub provider_id: Option<String>,
    pub enabled: bool,
}

/// A help document — SPEC §18.2 usability package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelpRegistration {
    pub capability_id: String,
    pub version: String,
    /// What the capability helps to accomplish.
    pub purpose: String,
    /// The key methods and why they are useful.
    pub key_methods: Vec<String>,
    /// The conditions under which the guidance holds.
    pub applies_when: Vec<String>,
    /// The cases it does not apply to.
    pub does_not_apply_when: Vec<String>,
    /// Schema and version help.
    pub schema_help: Option<String>,
    /// The known limits of this exact version.
    pub limitations: Vec<String>,
    /// How to recover when the capability fails.
    pub recovery: Option<String>,
    /// The implementation this document describes.
    pub implementation: VersionedRef,
}

/// The ownership token of one provider registration.
///
/// A token does not keep a withdrawn registration alive, and a stale token
/// cannot withdraw a later registration of the same id.
#[derive(Debug, Clone)]
pub struct ProviderHandle {
    provider_id: String,
    epoch: u64,
}

impl ProviderHandle {
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
}

/// The ownership token of one capability registration.
#[derive(Debug, Clone)]
pub struct CapabilityHandle {
    capability_id: String,
    epoch: u64,
}

impl CapabilityHandle {
    pub fn capability_id(&self) -> &str {
        &self.capability_id
    }
}

/// The ownership token of one help document.
#[derive(Debug, Clone)]
pub struct HelpHandle {
    capability_id: String,
    epoch: u64,
}

impl HelpHandle {
    pub fn capability_id(&self) -> &str {
        &self.capability_id
    }
}

/// A refused registration. The live view stays as it was.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum RegistrationError {
    #[error("provider {0} is already registered")]
    DuplicateProvider(String),
    #[error("capability {0} is already registered")]
    DuplicateCapability(String),
    #[error("help document for capability {0} is already registered")]
    DuplicateHelp(String),
    #[error("provider {0} is not registered")]
    UnknownProvider(String),
    #[error("registration {kind} {id} has an empty identity")]
    EmptyIdentity { kind: &'static str, id: String },
}

#[derive(Debug, Clone)]
struct ProviderEntry {
    view: ProviderView,
    epoch: u64,
}

/// One observed change of the live registry.
///
/// The change log is what lets a caller (the assembly root, a discovery
/// index, a UI) learn that the live view moved without polling it. It is a
/// projection of the same registration events, not a second source of truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RuntimeChange {
    ProviderRegistered {
        provider_id: String,
        provider_kind: String,
        version: String,
    },
    ProviderWithdrawn {
        provider_id: String,
    },
    CapabilityRegistered {
        capability_id: String,
        version: String,
        provider_id: Option<String>,
    },
    CapabilityWithdrawn {
        capability_id: String,
    },
    CapabilityEnabled {
        capability_id: String,
        enabled: bool,
    },
    ProviderHealthChanged {
        provider_id: String,
        health: ProviderHealth,
    },
}

#[derive(Debug, Clone)]
struct CapabilityEntry {
    view: CapabilityView,
    epoch: u64,
}

#[derive(Debug, Clone)]
struct HelpEntry {
    view: HelpView,
    epoch: u64,
}

#[derive(Debug, Default)]
struct RuntimeState {
    revision: u64,
    capability_revision: u64,
    providers: BTreeMap<String, ProviderEntry>,
    capabilities: BTreeMap<String, CapabilityEntry>,
    help: BTreeMap<String, HelpEntry>,
    changes: Vec<RuntimeChange>,
}

/// The live plugin runtime registry.
pub struct PluginRuntime {
    state: RwLock<RuntimeState>,
    /// The live hook graph, when the runtime publishes them together. The
    /// registry produces capability events into it; the graph dispatches
    /// them at the next defined boundary.
    hooks: RwLock<Option<Arc<HookRegistry>>>,
    /// The revision, published to whoever watches the live registry. A
    /// watcher sees every change, including one that happened before it
    /// started watching.
    revisions: tokio::sync::watch::Sender<u64>,
}

impl cordis_core::Service for PluginRuntime {
    const NAME: &'static str = PLUGIN_RUNTIME_SERVICE;
}

impl PluginRuntime {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(RuntimeState::default()),
            hooks: RwLock::new(None),
            revisions: tokio::sync::watch::channel(0).0,
        }
    }

    /// Watch the live registry.
    ///
    /// The receiver carries the registry revision; every registration,
    /// withdrawal and health change advances it. A watcher that starts late
    /// still observes the current revision, so a change is never lost
    /// between two reads.
    pub fn changed(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revisions.subscribe()
    }

    /// Publish the current revision to the watchers.
    fn publish_revision(&self, revision: u64) {
        self.revisions.send_replace(revision);
    }

    /// Bind the live hook graph to this registry.
    ///
    /// From then on a capability that appears, disappears or changes its
    /// enabled state produces a `runtime.capability_changed` event in the
    /// graph. The registry does not own an async runtime of its own, so the
    /// event is queued and dispatched at the next defined boundary.
    pub fn attach_hook_graph(&self, hooks: Arc<HookRegistry>) {
        *self.hooks.write() = Some(hooks);
    }

    /// The observed changes since the last drain.
    pub fn drain_changes(&self) -> Vec<RuntimeChange> {
        std::mem::take(&mut self.state.write().changes)
    }

    /// Produce the capability-changed event of one capability.
    fn notify_capability(&self, capability_id: &str, enabled: bool) {
        let hooks = self.hooks.read().clone();
        if let Some(hooks) = hooks {
            hooks.enqueue(HookPayload::CapabilityChanged {
                capability_id: capability_id.to_owned(),
                enabled,
            });
        }
    }

    /// The revision of the whole registry. Every registration, withdrawal and
    /// health change advances it.
    pub fn revision(&self) -> u64 {
        self.state.read().revision
    }

    /// The revision of the capability index. It advances when a capability or
    /// its help document changes, and when a provider's capability set or
    /// health changes.
    pub fn capability_revision(&self) -> u64 {
        self.state.read().capability_revision
    }

    /// Register or replace one provider.
    ///
    /// Replacing a provider of the same id is how an upgrade publishes its
    /// new implementation: the view names the new artifact and the revision
    /// advances.
    pub fn register_provider(
        &self,
        registration: ProviderRegistration,
    ) -> Result<ProviderHandle, RegistrationError> {
        if registration.provider_id.trim().is_empty() {
            return Err(RegistrationError::EmptyIdentity {
                kind: "provider",
                id: registration.provider_id,
            });
        }
        let mut state = self.state.write();
        state.revision += 1;
        state.capability_revision += 1;
        let epoch = state.revision;
        let kind = registration.kind.clone();
        let version = registration.version.clone();
        let view = ProviderView {
            provider_id: registration.provider_id.clone(),
            kind: registration.kind,
            version: registration.version,
            plugin_id: registration.plugin_id,
            implementation: registration.implementation,
            description: registration.description,
            operations: registration.operations,
            capabilities: registration.capabilities,
            health: registration.health,
        };
        state
            .providers
            .insert(registration.provider_id.clone(), ProviderEntry { view, epoch });
        state.changes.push(RuntimeChange::ProviderRegistered {
            provider_id: registration.provider_id.clone(),
            provider_kind: kind,
            version,
        });
        self.publish_revision(state.revision);
        Ok(ProviderHandle {
            provider_id: registration.provider_id,
            epoch,
        })
    }

    /// Withdraw exactly the provider this token owns.
    pub fn withdraw_provider(&self, handle: &ProviderHandle) -> bool {
        let mut state = self.state.write();
        let Some(entry) = state.providers.get(&handle.provider_id) else {
            return false;
        };
        if entry.epoch != handle.epoch {
            return false;
        }
        state.providers.remove(&handle.provider_id);
        state.changes.push(RuntimeChange::ProviderWithdrawn {
            provider_id: handle.provider_id.clone(),
        });
        state.revision += 1;
        state.capability_revision += 1;
        self.publish_revision(state.revision);
        true
    }

    /// Withdraw one provider by id, whoever registered it. Used by an
    /// uninstall that owns the whole plugin.
    pub fn remove_provider(&self, provider_id: &str) -> bool {
        let mut state = self.state.write();
        if state.providers.remove(provider_id).is_none() {
            return false;
        }
        state.changes.push(RuntimeChange::ProviderWithdrawn {
            provider_id: provider_id.to_owned(),
        });
        state.revision += 1;
        state.capability_revision += 1;
        self.publish_revision(state.revision);
        true
    }

    /// Record a new observed health for one provider.
    pub fn set_provider_health(
        &self,
        provider_id: &str,
        health: ProviderHealth,
    ) -> Result<(), RegistrationError> {
        let mut state = self.state.write();
        let Some(entry) = state.providers.get_mut(provider_id) else {
            return Err(RegistrationError::UnknownProvider(provider_id.to_owned()));
        };
        if entry.view.health == health {
            return Ok(());
        }
        entry.view.health = health;
        state.revision += 1;
        state.capability_revision += 1;
        state.changes.push(RuntimeChange::ProviderHealthChanged {
            provider_id: provider_id.to_owned(),
            health,
        });
        self.publish_revision(state.revision);
        Ok(())
    }

    /// Register or replace one capability.
    pub fn register_capability(
        &self,
        registration: CapabilityRegistration,
    ) -> Result<CapabilityHandle, RegistrationError> {
        if registration.capability_id.trim().is_empty() {
            return Err(RegistrationError::EmptyIdentity {
                kind: "capability",
                id: registration.capability_id,
            });
        }
        let mut state = self.state.write();
        state.revision += 1;
        state.capability_revision += 1;
        let epoch = state.revision;
        let version = registration.version.clone();
        let provider_id = registration.provider_id.clone();
        let enabled = registration.enabled;
        let view = CapabilityView {
            capability_id: registration.capability_id.clone(),
            version: registration.version,
            description: registration.description,
            plugin_id: registration.plugin_id,
            provider_id: registration.provider_id,
            enabled: registration.enabled,
        };
        state.capabilities.insert(
            registration.capability_id.clone(),
            CapabilityEntry { view, epoch },
        );
        state.changes.push(RuntimeChange::CapabilityRegistered {
            capability_id: registration.capability_id.clone(),
            version,
            provider_id,
        });
        let handle = CapabilityHandle {
            capability_id: registration.capability_id.clone(),
            epoch,
        };
        let revision = state.revision;
        drop(state);
        self.publish_revision(revision);
        self.notify_capability(&handle.capability_id, enabled);
        Ok(handle)
    }

    /// Withdraw exactly the capability this token owns.
    pub fn withdraw_capability(&self, handle: &CapabilityHandle) -> bool {
        let mut state = self.state.write();
        let Some(entry) = state.capabilities.get(&handle.capability_id) else {
            return false;
        };
        if entry.epoch != handle.epoch {
            return false;
        }
        state.capabilities.remove(&handle.capability_id);
        state.revision += 1;
        state.capability_revision += 1;
        state.changes.push(RuntimeChange::CapabilityWithdrawn {
            capability_id: handle.capability_id.clone(),
        });
        let revision = state.revision;
        drop(state);
        self.publish_revision(revision);
        self.notify_capability(&handle.capability_id, false);
        true
    }

    /// Register or replace one help document.
    pub fn register_help(
        &self,
        registration: HelpRegistration,
    ) -> Result<HelpHandle, RegistrationError> {
        if registration.capability_id.trim().is_empty() {
            return Err(RegistrationError::EmptyIdentity {
                kind: "help",
                id: registration.capability_id,
            });
        }
        let mut state = self.state.write();
        state.revision += 1;
        state.capability_revision += 1;
        let epoch = state.revision;
        let view = HelpView {
            capability_id: registration.capability_id.clone(),
            version: registration.version,
            purpose: registration.purpose,
            key_methods: registration.key_methods,
            applies_when: registration.applies_when,
            does_not_apply_when: registration.does_not_apply_when,
            schema_help: registration.schema_help,
            limitations: registration.limitations,
            recovery: registration.recovery,
            implementation: registration.implementation,
        };
        state
            .help
            .insert(registration.capability_id.clone(), HelpEntry { view, epoch });
        self.publish_revision(state.revision);
        Ok(HelpHandle {
            capability_id: registration.capability_id,
            epoch,
        })
    }

    /// Withdraw exactly the help document this token owns.
    pub fn withdraw_help(&self, handle: &HelpHandle) -> bool {
        let mut state = self.state.write();
        let Some(entry) = state.help.get(&handle.capability_id) else {
            return false;
        };
        if entry.epoch != handle.epoch {
            return false;
        }
        state.help.remove(&handle.capability_id);
        state.revision += 1;
        state.capability_revision += 1;
        self.publish_revision(state.revision);
        true
    }

    /// One provider of the live view.
    pub fn provider(&self, provider_id: &str) -> Option<ProviderView> {
        self.state
            .read()
            .providers
            .get(provider_id)
            .map(|entry| entry.view.clone())
    }

    /// One capability of the live view.
    pub fn capability(&self, capability_id: &str) -> Option<CapabilityView> {
        self.state
            .read()
            .capabilities
            .get(capability_id)
            .map(|entry| entry.view.clone())
    }

    /// One help document of the live view.
    pub fn help(&self, capability_id: &str) -> Option<HelpView> {
        self.state
            .read()
            .help
            .get(capability_id)
            .map(|entry| entry.view.clone())
    }

    /// The whole live view, with the revision it was read at.
    pub fn view(&self) -> RuntimeView {
        let state = self.state.read();
        RuntimeView {
            revision: state.revision,
            providers: state.providers.values().map(|entry| entry.view.clone()).collect(),
            capabilities: state
                .capabilities
                .values()
                .map(|entry| entry.view.clone())
                .collect(),
            help: state.help.values().map(|entry| entry.view.clone()).collect(),
        }
    }

    /// The live capabilities in the shape the subject's capability list uses.
    pub fn agent_capabilities(&self) -> Vec<AgentCapability> {
        self.state
            .read()
            .capabilities
            .values()
            .map(|entry| AgentCapability {
                capability_id: entry.view.capability_id.clone(),
                version: entry.view.version.clone(),
                description: entry.view.description.clone(),
                plugin_ref: Some(entry.view.plugin_id),
                enabled: entry.view.enabled,
            })
            .collect()
    }
}

impl Default for PluginRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Publishes the live runtime registry on the runtime's root context.
///
/// The registry is created here so the assembly root owns exactly one
/// instance; every plugin resolves it through the Cordis service lookup
/// instead of keeping its own copy.
pub struct PluginRuntimePlugin;

impl Plugin for PluginRuntimePlugin {
    type Config = ();
    type Input = Arc<PluginRuntime>;
    type PrepareError = std::convert::Infallible;
    type ApplyError = cordis_core::service::ServicePublishError;

    fn name(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("shenko-plugin-runtime")
    }

    fn prepare(&self, _config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(Arc::new(PluginRuntime::new()))
    }

    async fn apply(
        &self,
        ctx: Context,
        runtime: &Self::Input,
    ) -> Result<(), Self::ApplyError> {
        let _publication = ctx.provide(runtime.clone())?;
        Ok(())
    }
}

/// The same plugin prepared outside the assembly root.
pub fn default_plugin() -> PreparedPlugin {
    let plugin = PluginRuntimePlugin;
    let input = plugin.prepare(()).expect("infallible runtime preparation");
    PreparedPlugin::from_input(plugin, input)
}
