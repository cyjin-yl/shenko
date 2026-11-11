//! Built-in configuration namespaces — docs/specs/configuration-playground.md#spec-43-1.
//!
//! A namespace is how a plugin publishes one class of settings. Three facts
//! stay separate: the saved desired revision (`config_entries`), what a
//! consumer actually runs (the live value here), and each consumer's adoption
//! row. The namespaces in this module are owned by the web slice: each one
//! governs a consumer in this crate, so applying a value is observable in real
//! behaviour instead of merely stored.
//!
//! Registration is not a permission gate: `ConfigService::apply` records the
//! change and its receipt, and adoption is observed separately. A namespace
//! whose stored value cannot be trusted reports that instead of presenting the
//! compiled default as the consumer's actual value.

use std::sync::Arc;

use anyhow::{Result, bail};
use parking_lot::RwLock;
use serde_json::{Value, json};

use shenko_store::config::{ConfigAdoption, ConfigChange};
use shenko_store::store::Store;
use shenko_types::object::{Digest, SchemaRef};

use crate::config_service::{
    ConfigApplication, ConfigApplyOutcome, ConfigNamespace, ConfigNamespaceHandler, ConfigService,
    builtin_contract,
};

/// The namespace the served UI publishes its own settings under.
pub const UI_NAMESPACE: &str = "shenko.ui";
/// The default page bound of the timeline and context listing endpoints.
pub const PAGE_LIMIT_KEY: &str = "page_limit";
/// The compiled default, used only when no value was ever saved.
pub const DEFAULT_PAGE_LIMIT: usize = 50;
/// The hard upper bound of one page. A saved value above it is refused rather
/// than silently clamped: the stored revision must say what runs.
pub const MAX_PAGE_LIMIT: usize = 200;

const UI_CONTRACT: &[u8] =
    b"shenko.ui/v1: live web-consumer settings; hot reload; saved desired revision and consumer adoption are separate facts";
const UI_SCHEMA: &[u8] =
    b"shenko.ui.page_limit/v1: integer 1..=200; the default page bound of /api/tasks and /api/context";

/// One live, hot-reloadable value owned by an in-process consumer.
///
/// The value is read at the point of use (so a change takes effect without an
/// approval step or a restart), initialized from the durable entry at open
/// time (so a restart resumes the saved value), and each consumer's adoption
/// is recorded when the apply contract ran.
pub struct UiPageLimitNamespace {
    store: Arc<Store>,
    live: RwLock<usize>,
    /// A saved value that exists but cannot be trusted. While set, `read`
    /// reports the failure: the compiled default is what the process falls
    /// back to, and it is not presented as the consumer's actual value.
    load_error: RwLock<Option<String>>,
}

impl UiPageLimitNamespace {
    /// Open the namespace against the durable entry, so a restart continues
    /// the saved value rather than the compiled default.
    pub fn open(store: Arc<Store>) -> Self {
        let (live, load_error) = match store.config_entry(UI_NAMESPACE, PAGE_LIMIT_KEY) {
            Ok(Some(entry)) => match entry.desired_value_ref.as_ref() {
                None => (DEFAULT_PAGE_LIMIT, None),
                Some(reference) => match read_sealed(&store, reference) {
                    Ok(value) => match parse_page_limit(&value) {
                        Ok(limit) => (limit, None),
                        Err(error) => (DEFAULT_PAGE_LIMIT, Some(error)),
                    },
                    Err(error) => (DEFAULT_PAGE_LIMIT, Some(error)),
                },
            },
            Ok(None) => (DEFAULT_PAGE_LIMIT, None),
            Err(error) => (DEFAULT_PAGE_LIMIT, Some(format!("saved value unreadable: {error}"))),
        };
        Self {
            store,
            live: RwLock::new(live),
            load_error: RwLock::new(load_error),
        }
    }

    /// The bound the API applies right now.
    pub fn page_limit(&self) -> usize {
        *self.live.read()
    }

    fn seal_observation(&self, change: &ConfigChange, limit: usize) -> Result<shenko_types::object::ObjectRef> {
        let value = json!({
            "schema_version": 1,
            "namespace": &change.namespace,
            "logical_key": &change.logical_key,
            "change_id": change.change_id,
            "desired_revision": change.desired_revision,
            "live_value": limit,
            "consumers": CONSUMERS,
            "observed_at": chrono::Utc::now(),
        });
        Ok(self.store.seal_config_object(&value, "config-observation")?)
    }
}

/// The consumers that read this value.
const CONSUMERS: [&str; 2] = ["shenko-ui.api/tasks", "shenko-ui.api/context"];

impl ConfigNamespaceHandler for UiPageLimitNamespace {
    fn descriptor(&self) -> ConfigNamespace {
        ConfigNamespace {
            namespace: UI_NAMESPACE.to_string(),
            schema_ref: SchemaRef {
                name: "shenko.ui.page_limit/v1".into(),
                major: 1,
                digest: Digest::blake3(UI_SCHEMA),
            },
            owner_plugin: builtin_contract("shenko.ui", UI_CONTRACT),
            read_contract_ref: builtin_contract("shenko.ui.read", UI_CONTRACT),
            apply_contract_ref: builtin_contract("shenko.ui.apply", UI_CONTRACT),
            capability_refs: Vec::new(),
            application: ConfigApplication::HotReload,
            effect_contract_ref: builtin_contract("shenko.ui.effect", UI_CONTRACT),
            logical_keys: vec![PAGE_LIMIT_KEY.to_string()],
            description:
                "Served UI settings. page_limit is the default page bound of the timeline and context listing endpoints; the consumer reports the value it actually runs."
                    .into(),
        }
    }

    fn read(&self) -> Result<Value> {
        if let Some(error) = self.load_error.read().clone() {
            bail!("saved {UI_NAMESPACE}/{PAGE_LIMIT_KEY} is not usable: {error}");
        }
        Ok(json!(self.page_limit()))
    }

    fn consumers(&self) -> Vec<String> {
        CONSUMERS.iter().map(|consumer| consumer.to_string()).collect()
    }

    fn apply(&self, change: &ConfigChange, desired: &Value) -> Result<ConfigApplyOutcome> {
        let limit = parse_page_limit(desired).map_err(anyhow::Error::msg)?;
        let observation = self.seal_observation(change, limit)?;
        // The consumer reads this cell on its next request. Recording the
        // adoption here is the consumer's own statement that it now runs the
        // new revision — not an inference from the saved value.
        *self.live.write() = limit;
        *self.load_error.write() = None;
        let adopted = CONSUMERS
            .iter()
            .map(|consumer| ConfigAdoption {
                consumer_id: (*consumer).to_string(),
                adopted_revision: change.desired_revision,
                adopted_at_commit: change.updated_commit,
                detail: format!(
                    "reads {UI_NAMESPACE}/{PAGE_LIMIT_KEY} at the point of use; now {limit}"
                ),
            })
            .collect();
        Ok(ConfigApplyOutcome {
            adopted,
            observation_refs: vec![observation],
            detail: format!(
                "page limit {limit} is live for {}",
                CONSUMERS.join(", ")
            ),
        })
    }
}

/// Accept only an integer in `1..=MAX_PAGE_LIMIT`. A string, a float, a null
/// or an out-of-range number is refused instead of coerced.
fn parse_page_limit(desired: &Value) -> std::result::Result<usize, String> {
    let Some(number) = desired.as_u64() else {
        return Err(format!(
            "page_limit must be an integer, received {}",
            serde_json::to_string(desired).unwrap_or_else(|_| "<unprintable>".into())
        ));
    };
    let limit = usize::try_from(number).map_err(|_| "page_limit does not fit this platform".to_string())?;
    if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
        return Err(format!("page_limit must be 1..={MAX_PAGE_LIMIT}, received {limit}"));
    }
    Ok(limit)
}

fn read_sealed(
    store: &Store,
    reference: &shenko_types::object::ObjectRef,
) -> std::result::Result<Value, String> {
    let bytes = store
        .cas
        .read_object(&reference.digest)
        .map_err(|error| format!("saved value unavailable: {error}"))?;
    if bytes.len() as u64 != reference.bytes || Digest::blake3(&bytes) != reference.digest {
        return Err("saved value does not match its reference".into());
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("saved value is not JSON: {error}"))
}

/// The page bound a live consumer applies now.
///
/// `None` (no service, no namespace, an unreadable value) falls back to the
/// compiled default — the same value the process would use with no saved
/// configuration. It is never reported as an adoption.
pub fn page_limit(config: Option<&Arc<ConfigService>>) -> usize {
    config
        .and_then(|config| config.current_value(UI_NAMESPACE, PAGE_LIMIT_KEY))
        .and_then(|value| parse_page_limit(&value).ok())
        .unwrap_or(DEFAULT_PAGE_LIMIT)
}

/// Every namespace the web/playground slice owns. The configuration service
/// registers these on construction so an installation without any plugin still
/// has real, discoverable settings.
pub fn builtin_namespaces(store: &Arc<Store>) -> Vec<Arc<dyn ConfigNamespaceHandler>> {
    vec![Arc::new(UiPageLimitNamespace::open(Arc::clone(store)))]
}
