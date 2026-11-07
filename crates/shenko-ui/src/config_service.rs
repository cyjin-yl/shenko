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
    ConfigAdoption, ConfigAttemptOwner, ConfigChange, ConfigChangeDraft, ConfigEntry, ConfigReceipt,
};
use shenko_store::store::Store;
use shenko_types::ids::EntityId;
use shenko_types::object::{ArtifactRef, Digest, ObjectRef, SchemaRef, VersionedRef};

use crate::playground::stable_id;

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
    /// The saved desired value, resolved from its sealed reference. `None`
    /// means no value was ever saved (or the reference cannot be resolved,
    /// which the caller must not read as "the default was saved").
    pub desired_value: Option<Value>,
    /// The current value of the consumer that owns the key, when it can be
    /// read. It is `null` when the consumer cannot report it.
    pub current_value: Option<Value>,
    /// Why the consumer could not report its value. A consumer that cannot
    /// state what it runs (an unusable saved value, a withdrawn handler) is
    /// never reported as "no value"; the reason is part of the answer.
    pub current_value_error: Option<String>,
    pub adoptions: Vec<ConfigAdoption>,
    /// The newest committed change of this key that still has no receipt and
    /// was left behind by a process that is provably gone — an interrupted
    /// apply. It is reported as the unknown fact it is; reading a key never
    /// re-runs its handler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_change: Option<ConfigChange>,
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

/// The durable result of one apply attempt.
///
/// `replayed` is the honest answer to a repeated intent: the pair
/// `(origin_identity, logical_key)` already carried this exact desired value,
/// so the original change is returned and no second apply ran. A replay is
/// answered from durable facts — even if the namespace's handler has since
/// been replaced or withdrawn — because the intent was already recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigApplyResult {
    pub change: ConfigChange,
    pub replayed: bool,
}

/// What happened to one committed-but-unsettled configuration change when the
/// service looked at it — SPEC §43.1 (CFG02).
///
/// `settled` is true only when a durable receipt was recorded. The outcome
/// never claims an effect that was not observed: a dead attempt owner leaves
/// the change `unknown`, whether or not the consumer happens to report the
/// requested value now.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRecovery {
    pub namespace: String,
    pub logical_key: String,
    pub change_id: EntityId,
    pub settled: bool,
    /// unknown | observed_equal | observed_different | unreadable |
    /// handler_missing | legacy_unowned | still_owned | attempt_unproven
    pub outcome: String,
    pub detail: String,
}

/// The result of one reconciliation pass over interrupted changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRecoveryReport {
    pub schema_version: u32,
    pub recovered: Vec<ConfigRecovery>,
    /// Changes this process must not settle because another process (or an
    /// owner whose liveness cannot be proven) still owns the attempt.
    pub still_owned: Vec<ConfigRecovery>,
    pub notes: Vec<String>,
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
    /// The one configuration service, with this slice's built-in namespaces
    /// already registered.
    ///
    /// Registering a namespace is not an approval step: it publishes the keys
    /// this plugin exposes and the contract its apply attempt follows. A
    /// namespace nobody registered is not advertised anywhere.
    pub fn new(store: Arc<Store>) -> Self {
        let service = Self {
            store,
            namespaces: RwLock::new(BTreeMap::new()),
        };
        for handler in crate::config_namespaces::builtin_namespaces(service.store()) {
            service.register(handler);
        }
        service
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
        // An apply that lost its process leaves a committed intent without a
        // receipt. Reading the key settles that fact from the consumer's own
        // report — never by re-running the handler — so the view below is
        // post-reconciliation and the unknown stays explicit.
        let interrupted_change = self.settle_interrupted(namespace, logical_key)?;
        let entry = self.store.config_entry(namespace, logical_key)?;
        let adoptions = self.store.config_adoptions(namespace, logical_key)?;
        let desired_value = match entry.as_ref().and_then(|entry| entry.desired_value_ref.as_ref()) {
            Some(reference) => Some(self.read_sealed(reference)?),
            None => None,
        };
        let (current_value, current_value_error) = match handler.read() {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        Ok(ConfigValueView {
            namespace: namespace.to_string(),
            logical_key: logical_key.to_string(),
            entry,
            desired_value,
            current_value,
            current_value_error,
            adoptions,
            interrupted_change,
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
        // A preview is a statement about current facts, so an interrupted apply
        // of this key must be reconciled first instead of being read as an
        // ordinary saved-but-unadopted revision.
        self.settle_interrupted(namespace, logical_key)?;
        let entry = self.store.config_entry(namespace, logical_key)?;
        let base_revision = entry.as_ref().map(|entry| entry.desired_revision).unwrap_or(0);
        let stale = expected_base_revision.is_some_and(|expected| expected != base_revision);
        // Three different facts, never one "before/after": the saved desired
        // value, the value a consumer runs now, and the candidate. A consumer
        // that cannot state its running value is `null`, not the saved value.
        let saved_value = match entry.as_ref().and_then(|entry| entry.desired_value_ref.as_ref()) {
            Some(reference) => Some(self.read_sealed(reference)?),
            None => None,
        };
        let current_value = handler.read().ok();
        let candidate_value_ref = self.store.seal_config_object(&desired, "config-value")?;
        let diff = json!({
            "saved_value": saved_value,
            "running_value": current_value,
            "candidate_value": desired,
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
    /// The order is deliberate and is the one SPEC §43.1 asks for: a repeated
    /// intent is answered from the durable change record **before** any
    /// revision check or handler call, the desired revision and its change
    /// record are then committed first, the namespace's own apply contract
    /// runs next, and the adoption and receipt are recorded last. A crash
    /// between those steps leaves a saved desired revision with no adoption —
    /// which is exactly what the panel reports, instead of a silent success.
    pub fn apply(&self, request: ConfigApplyRequest) -> Result<ConfigApplyResult, ConfigError> {
        if request.origin_identity.trim().is_empty() {
            return Err(ConfigError::Denied(
                "a configuration change needs an origin identity".into(),
            ));
        }
        // The recorded actor is a provenance fact, so the service re-checks it
        // instead of trusting whatever its caller constructed.
        self.verify_actor(&request.actor)?;
        // §43.1(1): dedupe on (origin, logical key) comes before the current
        // revision check, and the answer is the durable original, not a
        // recomputed intent. A different payload never reuses the same key.
        if let Some(existing) = self
            .store
            .config_change_by_origin(&request.origin_identity, &request.logical_key)?
        {
            if self.same_desired(&existing, &request.desired)? {
                // A reclaim of an intent whose process died must answer with
                // the reconciled record, not with a raw `applying` row: the
                // handler is never re-run, and an unprovable effect stays
                // unknown.
                let existing = match self.settle_change(&existing)? {
                    Settle::Settled { change, .. } => change,
                    Settle::Kept { .. } => existing,
                };
                return Ok(ConfigApplyResult {
                    change: existing,
                    replayed: true,
                });
            }
            return Err(ConfigError::LogicalKeyReused(format!(
                "'{}/{}' under origin '{}' already belongs to a different desired value",
                existing.namespace, existing.logical_key, existing.origin_identity
            )));
        }
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
            // Deterministic identity: the same intent re-applied after a crash
            // resolves the original change instead of minting a second one.
            change_id: replay_change_id(
                &request.origin_identity,
                &request.logical_key,
                &desired_patch_ref.digest,
            ),
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
            // The attempt ownership is part of the same transaction as the
            // intent. After a crash the change therefore always says which
            // process was inside the apply, and recovery can ask whether that
            // process is gone instead of guessing whether an effect happened.
            attempt: Some(self.attempt_owner()),
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
        Ok(ConfigApplyResult {
            change: self.store.finish_config_change(change.change_id, state, receipt, adopted)?,
            replayed: false,
        })
    }

    /// The current value a live consumer reports, or `None` when the
    /// namespace is not installed or its handler cannot answer.
    pub fn current_value(&self, namespace: &str, logical_key: &str) -> Option<Value> {
        self.handler(namespace).ok()?.read().ok()
    }

    /// Read one sealed JSON value, refusing a reference whose bytes no longer
    /// match its digest or declared length.
    fn read_sealed(&self, reference: &ObjectRef) -> Result<Value, ConfigError> {
        let bytes = self
            .store
            .cas
            .read_object(&reference.digest)
            .map_err(|error| ConfigError::Internal(format!("read sealed config value: {error}")))?;
        if bytes.len() as u64 != reference.bytes || Digest::blake3(&bytes) != reference.digest {
            return Err(ConfigError::Internal(format!(
                "sealed config value {} does not match its reference",
                reference.digest
            )));
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| ConfigError::Internal(format!("decode sealed config value: {error}")))
    }

    /// True when the change's sealed desired value is exactly this payload.
    fn same_desired(&self, change: &ConfigChange, desired: &Value) -> Result<bool, ConfigError> {
        let Some(reference) = change.desired_patch_ref.as_ref() else {
            return Ok(desired.is_null());
        };
        Ok(self.read_sealed(reference)? == *desired)
    }

    /// The adoption state of every consumer of one key — used by the panel
    /// and by the subject's own configuration reads.
    pub fn adoption_state(&self, namespace: &str, logical_key: &str) -> Result<Vec<ConfigAdoption>, ConfigError> {
        Ok(self.store.config_adoptions(namespace, logical_key)?)
    }

    /// Reconcile one key's interrupted apply, if any — SPEC §43.1 (CFG02).
    ///
    /// The rule is evidence, not optimism: only the change's recorded attempt
    /// owner can say whether the apply is still in someone's hands, and only a
    /// provably gone owner is settled. Settling reads the consumer's own report
    /// and records it as an observation; it never calls the namespace's apply
    /// contract, so no unsafe handler is repeated and no external effect is
    /// duplicated. What the consumer reports is a fact about *now*, not proof
    /// that this intent caused it, so the receipt is `unknown` — the honest
    /// answer when the evidence cannot settle the question.
    pub fn settle_interrupted(
        &self,
        namespace: &str,
        logical_key: &str,
    ) -> Result<Option<ConfigChange>, ConfigError> {
        let unsettled = self
            .store
            .unsettled_config_changes(Some(namespace), Some(logical_key))?;
        let Some(change) = unsettled.into_iter().next() else {
            return Ok(None);
        };
        match self.settle_change(&change)? {
            Settle::Settled { change, .. } => Ok(Some(change)),
            Settle::Kept { .. } => Ok(None),
        }
    }

    /// Reconcile every interrupted change whose namespace is registered in
    /// this process. Delegated executors call this deliberately; a key whose
    /// namespace is not installed is reported, not guessed at.
    pub fn recover_interrupted(&self) -> Result<ConfigRecoveryReport, ConfigError> {
        let mut recovered = Vec::new();
        let mut still_owned = Vec::new();
        let mut notes = Vec::new();
        for change in self.store.unsettled_config_changes(None, None)? {
            let entry = match self.settle_change(&change)? {
                Settle::Settled { change, outcome, detail } => {
                    recovered.push(ConfigRecovery {
                        namespace: change.namespace.clone(),
                        logical_key: change.logical_key.clone(),
                        change_id: change.change_id,
                        settled: true,
                        outcome,
                        detail,
                    });
                    continue;
                }
                Settle::Kept { outcome, detail } => (outcome, detail),
            };
            still_owned.push(ConfigRecovery {
                namespace: change.namespace.clone(),
                logical_key: change.logical_key.clone(),
                change_id: change.change_id,
                settled: false,
                outcome: entry.0,
                detail: entry.1,
            });
        }
        if recovered.is_empty() && still_owned.is_empty() {
            notes.push("no committed configuration change was left without a receipt".into());
        }
        Ok(ConfigRecoveryReport {
            schema_version: 1,
            recovered,
            still_owned,
            notes,
        })
    }

    /// Decide one unsettled change. `Kept` means this process must not touch
    /// it: its attempt owner is live, or its liveness cannot be proven.
    fn settle_change(&self, change: &ConfigChange) -> Result<Settle, ConfigError> {
        if let Some(owner) = change.attempt.as_ref() {
            match attempt_liveness(owner) {
                AttemptLiveness::Gone => {}
                AttemptLiveness::Live => {
                    return Ok(Settle::Kept {
                        outcome: "still_owned".into(),
                        detail: format!(
                            "attempt is owned by a live process (incarnation {}, pid {})",
                            owner.incarnation, owner.pid
                        ),
                    });
                }
                AttemptLiveness::Unproven => {
                    return Ok(Settle::Kept {
                        outcome: "attempt_unproven".into(),
                        detail: format!(
                            "attempt owner pid {} cannot be checked against a recorded start token, so this process will not settle it",
                            owner.pid
                        ),
                    });
                }
            }
        }
        let desired = match change.desired_patch_ref.as_ref() {
            Some(reference) => Some(self.read_sealed(reference)?),
            None => None,
        };
        let handler = self.namespaces.read().get(&change.namespace).cloned();
        let (outcome, detail, reported) = match (change.attempt.as_ref(), handler.as_ref()) {
            (None, _) => (
                "legacy_unowned".to_string(),
                "this change was committed without attempt ownership, so whether its apply contract ran is not provable; its desired revision stays saved and no consumer adoption was recorded".to_string(),
                None,
            ),
            (Some(owner), None) => (
                "handler_missing".to_string(),
                format!(
                    "the attempt owner (incarnation {}, pid {}) is gone and namespace '{}' has no handler in this process, so the consumer cannot report its current value",
                    owner.incarnation, owner.pid, change.namespace
                ),
                None,
            ),
            (Some(owner), Some(handler)) => match handler.read() {
                Ok(current) => {
                    let equal = desired.as_ref() == Some(&current);
                    let (outcome, detail) = if equal {
                        (
                            "observed_equal".to_string(),
                            format!(
                                "the attempt owner (incarnation {}, pid {}) is gone; the consumer now reports the value this intent asked for. That is a fact about the consumer, not proof this change produced it, and no adoption row exists because the attempt never finished",
                                owner.incarnation, owner.pid
                            ),
                        )
                    } else {
                        let asked = desired
                            .as_ref()
                            .map_or_else(|| "no value".to_string(), |value| value.to_string());
                        (
                            "observed_different".to_string(),
                            format!(
                                "the attempt owner (incarnation {}, pid {}) is gone; the consumer reports {} while this intent asked for {}. The desired revision stays saved and unadopted; whether the apply contract ran before the interruption is not provable",
                                owner.incarnation,
                                owner.pid,
                                current,
                                asked,
                            )
                        )
                    };
                    (outcome, detail, Some(current))
                }
                Err(error) => (
                    "unreadable".to_string(),
                    format!(
                        "the attempt owner is gone and the consumer cannot report its current value: {error:#}"
                    ),
                    None,
                ),
            },
        };
        let observation = self.store.seal_config_object(
            &json!({
                "schema_version": 1,
                "change_id": change.change_id,
                "namespace": &change.namespace,
                "logical_key": &change.logical_key,
                "desired_revision": change.desired_revision,
                "desired_value": desired,
                "consumer_reported_value": reported,
                "attempt": &change.attempt,
                "outcome": &outcome,
                "observed_at": chrono::Utc::now(),
            }),
            "config-recovery-observation",
        )?;
        let receipt = ConfigReceipt {
            receipt_id: EntityId::new(),
            change_id: change.change_id,
            outcome: outcome.clone(),
            detail: detail.clone(),
            observation_refs: vec![observation],
            recorded_at: change.updated_commit,
        };
        let settled = self
            .store
            .finish_config_change(change.change_id, "unknown", receipt, Vec::new())?;
        Ok(Settle::Settled {
            change: settled,
            outcome,
            detail,
        })
    }

    /// The attempt identity this process commits with its intent.
    fn attempt_owner(&self) -> ConfigAttemptOwner {
        let pid = std::process::id();
        ConfigAttemptOwner {
            schema_version: 1,
            incarnation: self.store.capabilities.incarnation.to_string(),
            pid,
            process_start_token: process_start_token(pid).unwrap_or_default(),
            started_at: chrono::Utc::now(),
        }
    }

    /// Refuse to record provenance the caller merely asserts.
    ///
    /// A panel change must carry the installation's recognized owner
    /// principal. A subject change must name the delegated task it runs as;
    /// `config.manage` freezes that identity from the invocation, and no HTTP
    /// body can express either variant.
    fn verify_actor(&self, actor: &ConfigActor) -> Result<(), ConfigError> {
        match actor {
            ConfigActor::Owner { principal_ref } => {
                let installation_id = self.store.ledger.ledger_id();
                let grant = self
                    .store
                    .owner_grant(installation_id)?
                    .ok_or_else(|| {
                        ConfigError::Denied(
                            "this installation has no recognized owner, so no owner actor can be recorded"
                                .into(),
                        )
                    })?;
                if &grant.human_principal_ref != principal_ref {
                    return Err(ConfigError::Denied(
                        "the claimed owner principal is not this installation's recognized owner"
                            .into(),
                    ));
                }
                Ok(())
            }
            ConfigActor::Subject { task_id, .. } => {
                if *task_id == EntityId::nil() {
                    return Err(ConfigError::Denied(
                        "a subject actor must name the delegated task it runs as".into(),
                    ));
                }
                Ok(())
            }
        }
    }
}

/// The outcome of deciding one unsettled change.
enum Settle {
    Settled {
        change: ConfigChange,
        outcome: String,
        detail: String,
    },
    Kept {
        outcome: String,
        detail: String,
    },
}

/// Whether the process that owns an attempt is still the same live process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptLiveness {
    Live,
    Gone,
    Unproven,
}

fn attempt_liveness(owner: &ConfigAttemptOwner) -> AttemptLiveness {
    if owner.process_start_token.is_empty() {
        return AttemptLiveness::Unproven;
    }
    match process_start_token(owner.pid) {
        // Same pid and same kernel start token: the same process is alive.
        Some(token) if token == owner.process_start_token => AttemptLiveness::Live,
        // A different start token means the pid was reused by a new process.
        Some(_) => AttemptLiveness::Gone,
        None => AttemptLiveness::Gone,
    }
}

/// `/proc/<pid>/stat` field 22 — the kernel's same-process token. The
/// executable image is deliberately NOT part of this test: an upgrade may
/// replace the binary while the process keeps running.
#[cfg(target_os = "linux")]
fn process_start_token(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit(") ").next()?;
    after_comm.split_whitespace().nth(19).map(str::to_string)
}

#[cfg(not(target_os = "linux"))]
fn process_start_token(_pid: u32) -> Option<String> {
    None
}

/// The identity of one configuration intent.
///
/// It is derived from the origin, the logical key and the exact desired
/// bytes, so a replay after a crash names the same change. A different
/// payload under the same origin+key is a conflict (the store's unique index
/// enforces it), never an overwrite through a reused identity.
fn replay_change_id(origin_identity: &str, logical_key: &str, desired_digest: &Digest) -> EntityId {
    stable_id(&format!(
        "config-change/v1:{origin_identity}:{logical_key}:{}",
        desired_digest.0
    ))
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

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
