//! The configuration service — SPEC §43.1.
//!
//! One service owns configuration: the human panel, the subject's own
//! configuration intents and every plugin that registers a namespace all go
//! through [`ConfigService`]. There is no second store and no approval gate:
//! a change is admitted, applied by the namespace's own apply contract and
//! reported with a durable revision and receipt.
//!
//! Three facts stay separate and are never collapsed into one "ok":
//!
//! * the **saved desired revision** — what the installation asked for;
//! * the **adopted revision** — what each consumer actually runs;
//! * the **observation** — what the apply attempt actually observed.
//!
//! A saved value is not an applied value, and an applied value is not a
//! healthy one. Nothing here reports a check as passed when no check ran.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use shenko_store::config::{
    ConfigAdoption, ConfigChange, ConfigChangeDraft, ConfigEntry, ConfigReceipt,
};
use shenko_store::store::Store;
use shenko_types::ids::EntityId;
use shenko_types::object::{ArtifactRef, Digest, ObjectRef, SchemaRef, VersionedRef};

/// How a namespace's changes take effect — SPEC §43.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigApplication {
    HotReload,
    RestartService,
    GenerationHandoff,
}

impl ConfigApplication {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HotReload => "hot_reload",
            Self::RestartService => "restart_service",
            Self::GenerationHandoff => "generation_handoff",
        }
    }
}

/// One registered configuration namespace — SPEC §43.1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigNamespace {
    pub namespace: String,
    /// The schema of the values this namespace accepts.
    pub schema_ref: SchemaRef,
    /// The plugin that owns the namespace and its values.
    pub owner_plugin: VersionedRef,
    pub read_contract_ref: VersionedRef,
    pub apply_contract_ref: VersionedRef,
    /// Discoverable check, observation and recovery capabilities. A namespace
    /// may legitimately have none.
    pub capability_refs: Vec<ObjectRef>,
    pub application: ConfigApplication,
    pub effect_contract_ref: VersionedRef,
    /// The logical keys this namespace publishes.
    pub logical_keys: Vec<String>,
    pub description: String,
}

/// What a namespace's apply attempt actually did.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigApplyOutcome {
    pub adopted: Vec<ConfigAdoption>,
    pub observation_refs: Vec<ObjectRef>,
    pub detail: String,
}

/// The runtime contract of one namespace. Plugins implement it; the service
/// owns the durable facts.
pub trait ConfigNamespaceHandler: Send + Sync {
    fn descriptor(&self) -> ConfigNamespace;

    /// The value this consumer actually runs right now.
    fn read(&self) -> Result<Value>;

    /// The consumers that must adopt a new revision of this namespace.
    fn consumers(&self) -> Vec<String>;

    /// Apply the desired value. The handler owns the real effect; the
    /// service records what it observed.
    fn apply(&self, change: &ConfigChange, desired: &Value) -> Result<ConfigApplyOutcome>;
}

/// Who is performing a configuration change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigActor {
    /// The recognized owner, acting through an authenticated session.
    Owner { principal_ref: ObjectRef },
    /// The subject, acting through a delegated task.
    Subject {
        task_id: EntityId,
        delegation_ref: Option<ObjectRef>,
    },
}

impl ConfigActor {
    fn describe(&self) -> Value {
        match self {
            Self::Owner { principal_ref } => json!({
                "kind": "owner", "principal_ref": principal_ref,
            }),
            Self::Subject { task_id, delegation_ref } => json!({
                "kind": "subject", "task_id": task_id, "delegation_ref": delegation_ref,
            }),
        }
    }
}

/// The saved value plus what the consumers actually adopted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigValueView {
    pub namespace: String,
    pub logical_key: String,
    pub entry: Option<ConfigEntry>,
    /// The current value of the consumer that owns the key, when it can be
    /// read. It is `null` when the consumer cannot report it.
    pub current_value: Option<Value>,
    pub adoptions: Vec<ConfigAdoption>,
}

/// The result of looking at a change before making it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigPreview {
    pub namespace: String,
    pub logical_key: String,
    pub base_revision: u64,
    /// True when the caller's expected base revision no longer matches the
    /// saved desired revision.
    pub stale: bool,
    pub current_value: Option<Value>,
    /// The sealed candidate value. It is a candidate, not an adoption.
    pub candidate_value_ref: ObjectRef,
    pub diff: Value,
    pub affected_consumers: Vec<String>,
}

/// A request to apply one configuration value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigApplyRequest {
    pub namespace: String,
    pub logical_key: String,
    /// The idempotency identity of the origin: an origin event or task id.
    pub origin_identity: String,
    pub actor: ConfigActor,
    pub desired: Value,
    /// The revision the caller believes is current. `None` means "whatever is
    /// current now" — an explicit last-writer intent, not a silent overwrite
    /// of a change the caller never saw.
    pub expected_base_revision: Option<u64>,
    pub coordinating_task_id: Option<EntityId>,
}

/// The typed failures of the configuration service.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("CONFIG_NAMESPACE_UNKNOWN: {0}")]
    UnknownNamespace(String),
    #[error("CONFIG_KEY_UNKNOWN: {0}")]
    UnknownKey(String),
    #[error("BASE_CONFIG_STALE: {0}")]
    BaseConfigStale(String),
    #[error("CONFIG_LOGICAL_KEY_REUSED: {0}")]
    LogicalKeyReused(String),
    #[error("CONFIG_DENIED: {0}")]
    Denied(String),
    #[error("CONFIG_INTERNAL: {0}")]
    Internal(String),
}

impl ConfigError {
    /// The stable error code used by the HTTP and CLI surfaces.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownNamespace(_) => "CONFIG_NAMESPACE_UNKNOWN",
            Self::UnknownKey(_) => "CONFIG_KEY_UNKNOWN",
            Self::BaseConfigStale(_) => "BASE_CONFIG_STALE",
            Self::LogicalKeyReused(_) => "CONFIG_LOGICAL_KEY_REUSED",
            Self::Denied(_) => "CONFIG_DENIED",
            Self::Internal(_) => "CONFIG_INTERNAL",
        }
    }
}

impl From<anyhow::Error> for ConfigError {
    fn from(error: anyhow::Error) -> Self {
        let text = error.to_string();
        if text.contains("BASE_CONFIG_STALE") {
            Self::BaseConfigStale(text)
        } else if text.contains("CONFIG_LOGICAL_KEY_REUSED") {
            Self::LogicalKeyReused(text)
        } else {
            Self::Internal(text)
        }
    }
}

/// The one configuration service.
pub struct ConfigService {
    store: Arc<Store>,
    namespaces: RwLock<BTreeMap<String, Arc<dyn ConfigNamespaceHandler>>>,
}

impl ConfigService {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            namespaces: RwLock::new(BTreeMap::new()),
        }
    }

    /// Register a namespace. A later registration of the same name replaces
    /// the earlier one — the registry holds the live handler, not history.
    pub fn register(&self, handler: Arc<dyn ConfigNamespaceHandler>) {
        let namespace = handler.descriptor().namespace;
        self.namespaces.write().insert(namespace, handler);
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Every registered namespace — the discovery surface.
    pub fn discover(&self) -> Vec<ConfigNamespace> {
        self.namespaces
            .read()
            .values()
            .map(|handler| handler.descriptor())
            .collect()
    }

    fn handler(&self, namespace: &str) -> Result<Arc<dyn ConfigNamespaceHandler>, ConfigError> {
        self.namespaces
            .read()
            .get(namespace)
            .cloned()
            .ok_or_else(|| ConfigError::UnknownNamespace(namespace.to_string()))
    }

    /// Read one configuration key: the saved desired value and what the
    /// consumers actually adopted.
    pub fn read(&self, namespace: &str, logical_key: &str) -> Result<ConfigValueView, ConfigError> {
        let handler = self.handler(namespace)?;
        let descriptor = handler.descriptor();
        if !descriptor.logical_keys.is_empty()
            && !descriptor.logical_keys.iter().any(|key| key == logical_key)
        {
            return Err(ConfigError::UnknownKey(format!("{namespace}/{logical_key}")));
        }
        let entry = self.store.config_entry(namespace, logical_key)?;
        let adoptions = self.store.config_adoptions(namespace, logical_key)?;
        let current_value = handler.read().ok();
        Ok(ConfigValueView {
            namespace: namespace.to_string(),
            logical_key: logical_key.to_string(),
            entry,
            current_value,
            adoptions,
        })
    }

    /// Look at a change before making it. The preview never writes a durable
    /// desired revision and never claims a consumer adopted anything.
    pub fn preview(
        &self,
        namespace: &str,
        logical_key: &str,
        desired: Value,
        expected_base_revision: Option<u64>,
    ) -> Result<ConfigPreview, ConfigError> {
        let handler = self.handler(namespace)?;
        let descriptor = handler.descriptor();
        if !descriptor.logical_keys.is_empty()
            && !descriptor.logical_keys.iter().any(|key| key == logical_key)
        {
            return Err(ConfigError::UnknownKey(format!("{namespace}/{logical_key}")));
        }
        let entry = self.store.config_entry(namespace, logical_key)?;
        let base_revision = entry.as_ref().map(|entry| entry.desired_revision).unwrap_or(0);
        let stale = expected_base_revision.is_some_and(|expected| expected != base_revision);
        let current_value = handler.read().ok();
        let candidate_value_ref = self.store.seal_config_object(&desired, "config-value")?;
        let diff = json!({
            "before": current_value,
            "after": desired,
        });
        Ok(ConfigPreview {
            namespace: namespace.to_string(),
            logical_key: logical_key.to_string(),
            base_revision,
            stale,
            current_value,
            candidate_value_ref,
            diff,
            affected_consumers: handler.consumers(),
        })
    }

    /// Apply one configuration value.
    ///
    /// The order is deliberate: the desired revision and its change record
    /// are committed first, then the namespace's own apply contract runs, then
    /// the adoption and the receipt are recorded. A crash between those steps
    /// leaves a saved desired revision with no adoption — which is exactly
    /// what the panel reports, instead of a silent success.
    pub fn apply(&self, request: ConfigApplyRequest) -> Result<ConfigChange, ConfigError> {
        let handler = self.handler(&request.namespace)?;
        let descriptor = handler.descriptor();
        if !descriptor.logical_keys.is_empty()
            && !descriptor.logical_keys.iter().any(|key| key == &request.logical_key)
        {
            return Err(ConfigError::UnknownKey(format!(
                "{}/{}",
                request.namespace, request.logical_key
            )));
        }
        if request.origin_identity.trim().is_empty() {
            return Err(ConfigError::Denied(
                "a configuration change needs an origin identity".into(),
            ));
        }
        let entry = self.store.config_entry(&request.namespace, &request.logical_key)?;
        let base_revision = entry.as_ref().map(|entry| entry.desired_revision).unwrap_or(0);
        if let Some(expected) = request.expected_base_revision {
            if expected != base_revision {
                return Err(ConfigError::BaseConfigStale(format!(
                    "expected base revision {expected}, saved desired revision is {base_revision}"
                )));
            }
        }
        let desired_patch_ref = self.store.seal_config_object(&request.desired, "config-patch")?;
        let actor_ref = self
            .store
            .seal_config_object(&request.actor.describe(), "config-actor")?;
        let draft = ConfigChangeDraft {
            change_id: EntityId::new(),
            namespace: request.namespace.clone(),
            logical_key: request.logical_key.clone(),
            origin_identity: request.origin_identity.clone(),
            actor_and_authority_ref: actor_ref,
            target_refs: Vec::new(),
            desired_patch_ref: Some(desired_patch_ref),
            base_snapshot_ref: entry.as_ref().and_then(|entry| entry.desired_value_ref.clone()),
            candidate_snapshot_ref: None,
            coordinating_task_id: request.coordinating_task_id,
            operation_refs: Vec::new(),
            observation_refs: Vec::new(),
            recovery_program_refs: Vec::new(),
            activation_refs: Vec::new(),
            expected_base_revision: base_revision,
        };
        let change = self.store.commit_config_change(&draft)?;
        let outcome = handler.apply(&change, &request.desired);
        let (state, receipt, adopted) = match outcome {
            Ok(outcome) => {
                let receipt = ConfigReceipt {
                    receipt_id: EntityId::new(),
                    change_id: change.change_id,
                    outcome: "applied".into(),
                    detail: outcome.detail,
                    observation_refs: outcome.observation_refs.clone(),
                    recorded_at: change.updated_commit,
                };
                ("applied", receipt, outcome.adopted)
            }
            Err(error) => {
                // The desired value stays saved and unadopted. The failure is
                // a fact about the attempt, not a silent rollback.
                let receipt = ConfigReceipt {
                    receipt_id: EntityId::new(),
                    change_id: change.change_id,
                    outcome: "failed".into(),
                    detail: format!("{error:#}"),
                    observation_refs: Vec::new(),
                    recorded_at: change.updated_commit,
                };
                ("failed", receipt, Vec::new())
            }
        };
        Ok(self.store.finish_config_change(change.change_id, state, receipt, adopted)?)
    }

    /// The adoption state of every consumer of one key — used by the panel
    /// and by the subject's own configuration reads.
    pub fn adoption_state(&self, namespace: &str, logical_key: &str) -> Result<Vec<ConfigAdoption>, ConfigError> {
        Ok(self.store.config_adoptions(namespace, logical_key)?)
    }
}

/// Build a stable contract reference for a built-in namespace.
pub fn builtin_contract(name: &str, contract: &[u8]) -> VersionedRef {
    let digest = Digest::blake3(contract);
    VersionedRef {
        artifact: ArtifactRef {
            digest: digest.clone(),
            locator: format!("builtin:{name}"),
        },
        symbol: name.to_string(),
        schema: SchemaRef {
            name: format!("{name}-contract"),
            major: 1,
            digest: digest.clone(),
        },
    }
}
