//! Live configuration over the quota ledger and context policy registry.
//!
//! These adapters expose the existing consumers through `config.manage` and
//! the configuration panel. The quota ledger and PolicyRegistry remain the
//! authorities; saving a configuration is never substituted for their readback.
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use shenko_context_policy::PolicyRegistry;
use shenko_store::{
    Store,
    config::{ConfigAdoption, ConfigChange},
};
use shenko_types::object::{Digest, ObjectRef, SchemaRef};
use shenko_ui::config_service::{
    ConfigApplication, ConfigApplyOutcome, ConfigApplyRefusal, ConfigNamespace, ConfigNamespaceHandler,
    builtin_contract,
};

pub const QUOTA_NAMESPACE: &str = "shenko.quota";
pub const QUOTA_KEY: &str = "policy";
pub const CONTEXT_NAMESPACE: &str = "shenko.context-policy";
pub const PREFERENCE_KEY: &str = "compaction_preference";
const CONTEXT_CONSUMER: &str = "shenko-context-policy.selection";

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
        // Reject fields the actual typed reader would discard, including
        // nested ones. Re-serializing cannot detect these reliably: valid
        // default-valued fields intentionally disappear from historical
        // policy bytes. This keeps the policy type as the sole schema.
        let mut ignored = Vec::new();
        let policy: shenko_quota::QuotaPolicy =
            serde_ignored::deserialize(desired, |path| ignored.push(path.to_string()))
                .context("resource policy must be one complete QuotaPolicy")
                .map_err(ConfigApplyRefusal::new)?;
        if !ignored.is_empty() {
            return Err(ConfigApplyRefusal::new(anyhow::anyhow!(
                "resource policy contains unknown fields: {}", ignored.join(", ")
            )).into());
        }
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
}

impl ContextPreferenceNamespace {
    pub fn open(store: Arc<Store>, registry: Arc<PolicyRegistry>) -> Result<Self> {
        let adoption = store
            .config_adoptions(CONTEXT_NAMESPACE, PREFERENCE_KEY)?
            .into_iter()
            .find(|adoption| adoption.consumer_id == CONTEXT_CONSUMER);
        if let Some(adoption) = adoption {
            let change = store
                .config_change_at_revision(
                    CONTEXT_NAMESPACE,
                    PREFERENCE_KEY,
                    adoption.adopted_revision,
                )?
                .context("adopted context preference has no durable change record")?;
            let reference = adopted_context_reference(&change, &adoption)?;
            let desired = read_context_value(&store.cas, reference)?;
            registry.apply_preference(parse_preference(&desired)?);
        }
        // No adoption means installation order still owns selection. A saved
        // failed or interrupted intent is visible through ConfigValueView,
        // but cannot become a successful adoption just because we restarted.
        // If a recorded adoption was unreadable, return above before the
        // subject's consumers can start with a silently substituted default.
        Ok(Self { store, registry })
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

fn adopted_context_reference<'a>(
    change: &'a ConfigChange,
    adoption: &ConfigAdoption,
) -> Result<&'a ObjectRef> {
    ensure!(
        change.namespace == CONTEXT_NAMESPACE
            && change.logical_key == PREFERENCE_KEY
            && change.desired_revision == adoption.adopted_revision
            && change.adopted.iter().any(|recorded| {
                recorded.consumer_id == CONTEXT_CONSUMER
                    && recorded.adopted_revision == adoption.adopted_revision
                    && recorded.adopted_at_commit == adoption.adopted_at_commit
            }),
        "context preference adoption does not match its durable change"
    );
    change
        .desired_patch_ref
        .as_ref()
        .context("adopted context preference has no sealed value")
}

fn read_context_value(
    cas: &shenko_store::cas::ContentStore,
    reference: &ObjectRef,
) -> Result<Value> {
    let bytes = cas.read_object(&reference.digest)?;
    ensure!(
        bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
        "context preference does not match its sealed reference"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

/// Offline/CLI inspection reports saved intent and recorded adoption, never
/// inventing a live registry observation or changing the installation.
pub fn inspect_context_preference(data_dir: &std::path::Path, default: &[String]) -> Result<Value> {
    let snapshot = Store::inspect_config(
        &data_dir.join("shenko.db"),
        CONTEXT_NAMESPACE,
        PREFERENCE_KEY,
    )?;
    let desired_revision = snapshot.entry.as_ref().map(|entry| entry.desired_revision);
    let desired_reference = snapshot
        .entry
        .as_ref()
        .and_then(|entry| entry.desired_value_ref.as_ref());
    let adoption = snapshot
        .adoptions
        .iter()
        .find(|adoption| adoption.consumer_id == CONTEXT_CONSUMER);
    let cas = if desired_reference.is_some() || adoption.is_some() {
        Some(shenko_store::cas::ContentStore::open_existing(
            data_dir.join("cas"),
        )?)
    } else {
        None
    };
    let inspect_value = |reference: &ObjectRef| -> (Option<Value>, Option<String>) {
        let read = || {
            read_context_value(
                cas.as_ref().context("configuration CAS unavailable")?,
                reference,
            )
        };
        match read() {
            Ok(value) => {
                let error = parse_preference(&value)
                    .err()
                    .map(|error| format!("{error:#}"));
                (Some(value), error)
            }
            Err(error) => (None, Some(format!("{error:#}"))),
        }
    };
    let (desired_value, desired_value_error) =
        desired_reference.map(inspect_value).unwrap_or((None, None));
    let (adopted_value, adopted_value_error) = match adoption {
        Some(adoption) => {
            let reference = snapshot
                .adopted_changes
                .iter()
                .find(|change| change.desired_revision == adoption.adopted_revision)
                .context("adopted context preference has no durable change record")
                .and_then(|change| adopted_context_reference(change, adoption));
            match reference {
                Ok(reference) => inspect_value(reference),
                Err(error) => (None, Some(format!("{error:#}"))),
            }
        }
        None => (None, None),
    };
    let (preference, source) = if adoption.is_some() {
        if adopted_value_error.is_none() {
            (adopted_value.clone(), "last recorded adoption")
        } else {
            (None, "last recorded adoption unavailable")
        }
    } else {
        (
            Some(json!(default)),
            "installation order (no recorded adoption)",
        )
    };
    Ok(json!({
        "preference": preference, "preference_source": source,
        "desired_revision": desired_revision,
        "desired_value": desired_value, "desired_value_error": desired_value_error,
        "desired_change_state": snapshot.desired_change.as_ref().map(|change| &change.state),
        "adopted_revision": adoption.map(|adoption| adoption.adopted_revision),
        "adopted_value": adopted_value, "adopted_value_error": adopted_value_error,
        "adoptions": snapshot.adoptions,
        "runtime_observed": false,
    }))
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
        Ok(json!(self.registry.preference()))
    }

    fn consumers(&self) -> Vec<String> {
        vec![CONTEXT_CONSUMER.into()]
    }

    fn apply(&self, change: &ConfigChange, desired: &Value) -> Result<ConfigApplyOutcome> {
        let preference = parse_preference(desired).map_err(ConfigApplyRefusal::new)?;
        let adoption = self.registry.apply_preference(preference);
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
