//! Live policy configuration — docs/specs/configuration-playground.md#spec-43-1,
//! docs/specs/quota-resources.md#spec-42 and docs/specs/context-policies.md#spec-12-3-1.
//!
//! These adapters expose the existing consumers through `config.manage` and
//! the configuration panel. The quota ledger and PolicyRegistry remain the
//! authorities; saving a configuration is never substituted for their readback.
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use parking_lot::RwLock;
use serde_json::{Value, json};
use shenko_context_policy::PolicyRegistry;
use shenko_store::{
    Store,
    config::{ConfigAdoption, ConfigChange},
};
use shenko_types::object::{Digest, SchemaRef};
use shenko_ui::config_service::{
    ConfigApplication, ConfigApplyOutcome, ConfigNamespace, ConfigNamespaceHandler,
    builtin_contract,
};

pub const QUOTA_NAMESPACE: &str = "shenko.quota";
pub const QUOTA_KEY: &str = "policy";
pub const CONTEXT_NAMESPACE: &str = "shenko.context-policy";
pub const PREFERENCE_KEY: &str = "compaction_preference";

const QUOTA_CONTRACT: &[u8] = b"shenko.quota.policy/v1: complete QuotaPolicy; append immutable policy identity/revision; readback latest durable policy; applies to subsequent admissions and observations";
const CONTEXT_CONTRACT: &[u8] = b"shenko.context-policy.compaction_preference/v1: array of strategy ids; requested order is live; unlisted installed strategies follow; missing strategies remain visible in receipt; restore saved order on restart";

fn descriptor(namespace: &str, key: &str, contract: &[u8], description: &str) -> ConfigNamespace {
    ConfigNamespace {
        namespace: namespace.into(),
        schema_ref: SchemaRef {
            name: format!("{namespace}.{key}/v1"),
            major: 1,
            digest: Digest::blake3(contract),
        },
        owner_plugin: builtin_contract(namespace, contract),
        read_contract_ref: builtin_contract(&format!("{namespace}.read"), contract),
        apply_contract_ref: builtin_contract(&format!("{namespace}.apply"), contract),
        capability_refs: Vec::new(),
        application: ConfigApplication::HotReload,
        effect_contract_ref: builtin_contract(&format!("{namespace}.effect"), contract),
        logical_keys: vec![key.into()],
        description: description.into(),
    }
}

/// The same immutable policy ledger read by subject/child quota admission and
/// quota observation. An old policy replay is not a new selection: readback
/// below refuses to claim adoption if a later policy is still selected.
pub struct QuotaNamespace(pub Arc<Store>);

impl ConfigNamespaceHandler for QuotaNamespace {
    fn descriptor(&self) -> ConfigNamespace {
        descriptor(
            QUOTA_NAMESPACE,
            QUOTA_KEY,
            QUOTA_CONTRACT,
            "Resource policy for model admission and quota observations. Apply one complete QuotaPolicy with an unused policy identity/revision. The latest ledger policy is read at admission; already armed or dispatched sends keep their frozen accounting. Read this value first, preserve its fields, and change the intended policy inputs. This does not supply provider credentials or invent a quota reading.",
        )
    }

    fn read(&self) -> Result<Value> {
        Ok(serde_json::to_value(self.0.latest_quota_policy()?)?)
    }

    fn consumers(&self) -> Vec<String> {
        // Admission is composed for every subject/model child. Optional
        // account observers also read this ledger, but their installation
        // cannot be inferred merely from a policy being configured.
        vec!["shenko-subject.quota-admission".into()]
    }

    fn apply(&self, change: &ConfigChange, desired: &Value) -> Result<ConfigApplyOutcome> {
        let policy: shenko_quota::QuotaPolicy = serde_json::from_value(desired.clone())
            .context("resource policy must be one complete QuotaPolicy")?;
        // Deserialize/serialize equality also rejects misspelled inputs that
        // the older QuotaPolicy reader would otherwise silently discard.
        let normalized = serde_json::to_value(&policy)?;
        ensure!(
            desired
                .as_object()
                .is_some_and(
                    |object| object.keys().all(|key| normalized.get(key).is_some()
                        || matches!(
                            key.as_str(),
                            "objective"
                                | "target_utilization_range"
                                | "planning_horizon"
                                | "emergency_reserve_by_window"
                                | "uncertainty_margin"
                                | "admission_pacing"
                        ))
                ),
            "resource policy contains an unknown field"
        );
        let policy_receipt = self.0.record_quota_policy(&policy, chrono::Utc::now())?;
        ensure!(
            self.0.latest_quota_policy()?.as_ref() == Some(&policy),
            "this immutable policy revision is historical; append a new revision to select it again"
        );
        let detail = format!(
            "policy {} revision {} is the current quota ledger policy; subsequent admission and observation reads use it; existing armed sends retain their bound accounting",
            policy.policy_id, policy.revision,
        );
        let observation = self.0.seal_config_object(
            &json!({
                "schema_version": 1, "change_id": change.change_id,
                "desired_revision": change.desired_revision,
                "policy_commit": policy_receipt.commit_ref,
                "live_value": policy, "observed_at": chrono::Utc::now(),
            }),
            "config-observation",
        )?;
        Ok(ConfigApplyOutcome {
            adopted: self
                .consumers()
                .into_iter()
                .map(|consumer_id| ConfigAdoption {
                    consumer_id,
                    adopted_revision: change.desired_revision,
                    adopted_at_commit: policy_receipt.commit_ref,
                    detail: detail.clone(),
                })
                .collect(),
            observation_refs: vec![observation],
            detail,
        })
    }
}

/// The registry reads this preference at policy selection, so changing it is
/// a synchronous, observable consumer adoption. Unknown ids are preserved as
/// requested preference, without claiming the corresponding provider exists.
pub struct ContextPreferenceNamespace {
    store: Arc<Store>,
    registry: Arc<PolicyRegistry>,
    load_error: RwLock<Option<String>>,
}

impl ContextPreferenceNamespace {
    pub fn open(store: Arc<Store>, registry: Arc<PolicyRegistry>) -> Self {
        let restore = || -> Result<()> {
            let Some(entry) = store.config_entry(CONTEXT_NAMESPACE, PREFERENCE_KEY)? else {
                return Ok(());
            };
            let Some(reference) = entry.desired_value_ref else {
                return Ok(());
            };
            let bytes = store.cas.read_object(&reference.digest)?;
            ensure!(
                bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
                "saved context preference does not match its sealed reference"
            );
            let desired: Value = serde_json::from_slice(&bytes)?;
            registry.apply_preference(parse_preference(&desired)?);
            Ok(())
        };
        let load_error = restore().err().map(|error| format!("{error:#}"));
        Self {
            store,
            registry,
            load_error: RwLock::new(load_error),
        }
    }
}

fn parse_preference(desired: &Value) -> Result<Vec<String>> {
    let ids: Vec<String> = serde_json::from_value(desired.clone())
        .context("compaction preference must be an array of strategy ids")?;
    ensure!(
        ids.iter().all(|id| !id.trim().is_empty()),
        "a strategy id cannot be empty"
    );
    Ok(ids)
}

impl ConfigNamespaceHandler for ContextPreferenceNamespace {
    fn descriptor(&self) -> ConfigNamespace {
        descriptor(
            CONTEXT_NAMESPACE,
            PREFERENCE_KEY,
            CONTEXT_CONTRACT,
            "Context compaction strategy preference. Apply an array of strategy ids; omitted installed strategies follow in installation order. An unknown id stays in the requested order and is reported in the receipt, without making the strategy installed. The live registry adopts the order immediately and restores it on restart.",
        )
    }

    fn read(&self) -> Result<Value> {
        if let Some(error) = self.load_error.read().as_ref() {
            anyhow::bail!("saved context preference is unusable: {error}");
        }
        Ok(json!(self.registry.preference()))
    }

    fn consumers(&self) -> Vec<String> {
        vec!["shenko-context-policy.selection".into()]
    }

    fn apply(&self, change: &ConfigChange, desired: &Value) -> Result<ConfigApplyOutcome> {
        let adoption = self.registry.apply_preference(parse_preference(desired)?);
        *self.load_error.write() = None;
        let observation = self.store.seal_config_object(
            &json!({
                "schema_version": 1, "change_id": change.change_id,
                "desired_revision": change.desired_revision,
                "live_value": self.registry.preference(), "selection": adoption,
            }),
            "config-observation",
        )?;
        let detail = format!(
            "context selection adopted preference {:?}; effective installed order {:?}; unavailable strategy ids {:?}",
            adoption.requested, adoption.adopted, adoption.unknown
        );
        Ok(ConfigApplyOutcome {
            adopted: vec![ConfigAdoption {
                consumer_id: self.consumers().remove(0),
                adopted_revision: change.desired_revision,
                adopted_at_commit: change.updated_commit,
                detail: detail.clone(),
            }],
            observation_refs: vec![observation],
            detail,
        })
    }
}

#[cfg(test)]
#[path = "policy_config_tests.rs"]
mod tests;
