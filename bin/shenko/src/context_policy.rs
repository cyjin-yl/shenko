//! The installable context strategies and their runtime wiring — SPEC §12.3.1.
//!
//! Budget compact and snapcompact are two independent strategies. Each one
//! is an installable code unit: its manifest, version, dependencies and
//! enabled state live in the Node installation record, which is the
//! installation authority. The runtime reconciles the live registry against
//! that record, so installing, updating, disabling or replacing a strategy
//! is an ordinary change of an installed unit — not a second enable/version
//! registry of our own.
//!
//! The preference order is configuration, owned by the configuration service
//! (`shenko.context-policy` / `compaction_preference`); until that service
//! carries a value, the runtime order follows the installation order of the
//! enabled units and reports which source it used.
use std::path::Path;

use anyhow::{Context as _, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64_STD};
use serde_json::{Value, json};
use shenko_context_policy::{ContextPolicy, PolicyManifest};
use shenko_node::{
    Node, UnitDescriptor, UnitExport, UnitKind, UnitPackage, UnitRequest, UNIT_SCHEMA,
};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

use crate::{ContextCommand, ContextPolicyCommand};

/// The installation unit id of a context strategy.
pub fn unit_id(policy_id: &str) -> String {
    format!("policy.{policy_id}")
}

/// One installable strategy the runtime knows how to install.
pub struct PolicyEntry {
    pub id: &'static str,
    /// The strategy's default configuration document.
    pub default_config: Value,
    /// Build the strategy from a configuration document.
    pub build: fn(Value) -> Result<Box<dyn ContextPolicy>, String>,
}

/// Every built-in context strategy.
pub fn catalog() -> Vec<PolicyEntry> {
    vec![
        PolicyEntry {
            id: shenko_compact_budget::POLICY_ID,
            default_config: shenko_compact_budget::BudgetCompact::default_config(),
            build: |value| {
                let config = shenko_compact_budget::BudgetCompact::config_from_json(&value)?;
                Ok(Box::new(shenko_compact_budget::BudgetCompact::new(config))
                    as Box<dyn ContextPolicy>)
            },
        },
        PolicyEntry {
            id: shenko_compact_snap::POLICY_ID,
            default_config: shenko_compact_snap::SnapCompact::default_config(),
            build: |value| {
                let config = shenko_compact_snap::SnapCompact::config_from_json(&value)?;
                Ok(Box::new(shenko_compact_snap::SnapCompact::new(config))
                    as Box<dyn ContextPolicy>)
            },
        },
    ]
}

/// A strategy whose installation record exists on this node.
pub struct InstalledPolicy {
    pub id: String,
    pub unit_id: String,
    pub revision: String,
    /// Whether an installation record exists at all. A record that does not
    /// exist is not the same as a record that is disabled.
    pub installed: bool,
    pub enabled: bool,
    pub manifest: PolicyManifest,
    pub config: Value,
}

/// The runtime configuration of one installed strategy.
///
/// The plugin is spawned with exactly the configuration its installation
/// record carries, so a changed record changes the running strategy.
pub enum PolicyRuntime {
    Budget(shenko_compact_budget::BudgetConfig),
    Snap(shenko_compact_snap::SnapConfig),
}

/// The manifest document of a strategy — the artifact a built-in
/// implementation is identified by.
///
/// A strategy that is compiled into the product has no separate artifact
/// file; its manifest is the content that identifies the implementation, so
/// the unit's artifact digest is the digest of this document.
pub fn manifest_document(manifest: &PolicyManifest) -> Value {
    json!({
        "id": manifest.id.clone(),
        "version": manifest.version.clone(),
        "contract": manifest.contract,
        "requires": manifest.requires.clone(),
        "provides": manifest.provides.clone(),
        "summary": manifest.summary.clone(),
        "dependencies": manifest.dependencies.clone(),
    })
}

/// The installation records of the built-in strategies, read only.
///
/// This is a pure query: it never creates, migrates or writes anything. A
/// strategy whose record does not exist yet is reported as absent, which is
/// different from "installed but disabled".
pub fn installed_policies(node: &Node) -> Result<Vec<InstalledPolicy>> {
    let exports = node.export_installation_units()?;
    let mut result = Vec::new();
    for entry in catalog() {
        let policy = (entry.build)(entry.default_config.clone())
            .map_err(|error| anyhow::anyhow!("policy {} is not buildable: {error}", entry.id))?;
        let manifest = policy.manifest();
        let unit = unit_id(entry.id);
        let existing = exports.iter().find(|export| export.root_unit_id == unit);
        let (enabled, revision, config) = match existing {
            None => (false, String::new(), entry.default_config.clone()),
            Some(export) => {
                let config = export
                    .packages
                    .first()
                    .map(|package| package.unit.config.clone())
                    .filter(|value| value.is_object())
                    .unwrap_or_else(|| entry.default_config.clone());
                (
                    export.enabled,
                    export.root_revision.clone().unwrap_or_default(),
                    config,
                )
            }
        };
        result.push(InstalledPolicy {
            id: entry.id.to_string(),
            unit_id: unit,
            revision,
            installed: existing.is_some(),
            enabled,
            manifest,
            config,
        });
    }
    Ok(result)
}

/// Ensure the installation record of every built-in strategy exists and names
/// the version that is actually compiled in. An existing record keeps its
/// enabled state and its configuration; a missing one is installed enabled.
///
/// The record is the installation authority: the runtime installs a strategy
/// only while its unit is enabled.
pub fn reconcile_units(node: &mut Node, package_root: &Path) -> Result<Vec<InstalledPolicy>> {
    let installed = installed_policies(node)?;
    for policy in installed {
        let manifest = &policy.manifest;
        if policy.installed && policy.revision == manifest.version {
            continue;
        }
        // A record that does not exist yet is installed enabled; an existing
        // record keeps the state the owner gave it.
        let enabled = if policy.installed { policy.enabled } else { true };
        let document = manifest_document(manifest);
        let artifact = serde_json::to_vec(&document)?;
        let digest = Digest::blake3(&artifact);
        let export = UnitExport {
            schema: UNIT_SCHEMA,
            root_unit_id: policy.unit_id.clone(),
            root_revision: Some(manifest.version.clone()),
            root_artifact_digest: Some(digest.clone()),
            origin_node: None,
            enabled,
            packages: vec![UnitPackage {
                unit: UnitDescriptor {
                    unit_id: policy.unit_id.clone(),
                    kind: UnitKind::Code,
                    revision: manifest.version.clone(),
                    artifact_digest: digest.clone(),
                    provides: manifest.provides.clone(),
                    dependencies: Vec::new(),
                    config: policy.config.clone(),
                    applicability: None,
                },
                artifact_base64: B64_STD.encode(&artifact),
                evidence: Vec::new(),
                resolved_dependencies: None,
            }],
        };
        node.handle_unit_request(UnitRequest::Import {
            export,
            package_root: package_root.to_string_lossy().into_owned(),
            enable: Some(enabled),
        })
        .with_context(|| format!("install context policy unit {}", policy.unit_id))?;
    }
    installed_policies(node)
}

/// Install or disable one strategy's installation record.
pub fn set_enabled(node: &mut Node, policy_id: &str, enabled: bool) -> Result<Value> {
    let unit = unit_id(policy_id);
    node.handle_unit_request(UnitRequest::SetEnabled { unit_id: unit, enabled })
}

/// The strategies that are installed and enabled, in installation order.
pub fn enabled_policies(installed: &[InstalledPolicy]) -> Vec<&InstalledPolicy> {
    installed.iter().filter(|policy| policy.installed && policy.enabled).collect()
}

/// The strategy ids in the order they should be tried.
pub fn preference_order(installed: &[InstalledPolicy]) -> Vec<String> {
    enabled_policies(installed).iter().map(|policy| policy.id.clone()).collect()
}

/// The runtime configuration of an installed strategy, read from its own
/// installation record. A malformed configuration is an error, never a
/// silently ignored default.
pub fn runtime_config(policy: &InstalledPolicy) -> Result<PolicyRuntime, String> {
    match policy.id.as_str() {
        shenko_compact_budget::POLICY_ID => Ok(PolicyRuntime::Budget(
            shenko_compact_budget::BudgetCompact::config_from_json(&policy.config)?,
        )),
        shenko_compact_snap::POLICY_ID => Ok(PolicyRuntime::Snap(
            shenko_compact_snap::SnapCompact::config_from_json(&policy.config)?,
        )),
        other => Err(format!("unknown context strategy {other}")),
    }
}

/// The identity of a new scope, used when a chain is created for a branch.
pub fn new_scope_id() -> EntityId {
    EntityId::new()
}

/// The strategies this product can install, with their manifest facts.
pub fn strategy_report() -> Value {
    let mut strategies = Vec::new();
    for entry in catalog() {
        let policy = match (entry.build)(entry.default_config.clone()) {
            Ok(policy) => policy,
            Err(error) => {
                strategies.push(json!({"id": entry.id, "buildable": false, "error": error}));
                continue;
            }
        };
        let manifest = policy.manifest();
        let unit = unit_id(&manifest.id);
        strategies.push(json!({
            "id": manifest.id,
            "version": manifest.version,
            "contract": manifest.contract,
            "requires": manifest.requires,
            "provides": manifest.provides,
            "dependencies": manifest.dependencies,
            "summary": manifest.summary,
            "unit_id": unit,
            "default_config": entry.default_config,
        }));
    }
    Value::Array(strategies)
}

/// The installation record of the context strategies.
pub fn installation_report(node: &Node) -> Result<Value> {
    let exports = node.export_installation_units()?;
    let mut units = Vec::new();
    for export in exports {
        if !export.root_unit_id.starts_with("policy.") {
            continue;
        }
        units.push(json!({
            "unit_id": export.root_unit_id,
            "revision": export.root_revision,
            "artifact_digest": export.root_artifact_digest.map(|digest| digest.0),
            "enabled": export.enabled,
            "config": export.packages.first().map(|package| package.unit.config.clone()),
        }));
    }
    Ok(Value::Array(units))
}

/// The committed window chain of the subject scope, as metadata only:
/// derivative payloads are never read.
pub fn window_report(store: &std::sync::Arc<shenko_store::Store>, limit: u64) -> Result<Value> {
    let Some(agent_id) = store.get_existing_agent_id()? else {
        return Ok(json!({"available": false, "note": "subject is not initialized"}));
    };
    let (head, _) = store.load_subject(agent_id)?;
    let scope = EntityId(head.branch_id.0);
    let log = shenko_context_policy::StoreWindowLog::new(std::sync::Arc::clone(store), scope);
    let page = log.meta_page(None, limit.clamp(1, 200))?;
    let windows: Vec<Value> = page
        .windows
        .iter()
        .map(|meta| {
            let mut kinds = serde_json::Map::new();
            for entry in &meta.window.entries {
                let kind = match entry {
                    shenko_context_policy::WindowEntry::Verbatim { .. } => "verbatim",
                    shenko_context_policy::WindowEntry::Pointer { .. } => "pointer",
                    shenko_context_policy::WindowEntry::Text { .. } => "text",
                    shenko_context_policy::WindowEntry::Image { .. } => "image",
                };
                let count = kinds.entry(kind.to_string()).or_insert(json!(0));
                *count = json!(count.as_u64().unwrap_or(0) + 1);
            }
            json!({
                "id": meta.window.id.to_string(),
                "generation": meta.window.generation,
                "policy_id": meta.window.policy_id.clone(),
                "policy_version": meta.window.policy_version.clone(),
                "model_generation": meta.window.model_generation,
                "covers_through": meta.window.covers_through,
                "entry_kinds": Value::Object(kinds),
                "cost": meta.window.cost.clone(),
                "derivatives": meta.derivatives.iter().map(|derivative| json!({
                    "media_type": derivative.media_type.clone(),
                    "blake3": derivative.blake3.clone(),
                    "bytes": derivative.bytes,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "available": true,
        "scope": scope.to_string(),
        "model_generation": store.context_model_generation(scope)?,
        "profile_digest": store.context_profile(scope)?.map(|(_, digest)| digest),
        "window_total": page.total,
        "has_older": page.has_older,
        "windows": windows,
    }))
}

/// Run one `shenko context ...` command.
pub fn command(data_dir: &Path, command: ContextCommand) -> Result<()> {
    match command {
        ContextCommand::Policy { command } => policy_command(data_dir, command),
        ContextCommand::Windows { limit } => windows_command(data_dir, limit),
    }
}

/// The installed strategies and their installation record.
fn policy_command(data_dir: &Path, command: ContextPolicyCommand) -> Result<()> {
    match command {
        ContextPolicyCommand::List => {
            // A pure read: an installation without a node database simply has
            // no installed strategies.
            let Some(node) = Node::open_inspect(&data_dir.join("node"))? else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "available": false,
                        "note": "no node installation in this data directory",
                        "strategies": strategy_report(),
                    }))?
                );
                return Ok(());
            };
            let installed = installed_policies(&node)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "available": true,
                    "strategies": strategy_report(),
                    "installation": installation_report(&node)?,
                    "enabled": enabled_policies(&installed)
                        .iter()
                        .map(|policy| policy.id.clone())
                        .collect::<Vec<_>>(),
                    "preference": preference_order(&installed),
                    "preference_source": "installation order (the configuration service has no value yet)",
                }))?
            );
            Ok(())
        }
        ContextPolicyCommand::Enable { id } => set_policy_enabled(data_dir, &id, true),
        ContextPolicyCommand::Disable { id } => set_policy_enabled(data_dir, &id, false),
    }
}

/// Enable or disable one strategy's installation record.
fn set_policy_enabled(data_dir: &Path, id: &str, enabled: bool) -> Result<()> {
    let known = catalog().iter().any(|entry| entry.id == id);
    anyhow::ensure!(
        known,
        "unknown context strategy {id}; run `shenko context policy list` for the installable ones"
    );
    let db_path = data_dir.join("shenko.db");
    anyhow::ensure!(
        db_path.is_file(),
        "no managed installation at {}; run `shenko start` once",
        data_dir.display()
    );
    let store = shenko_store::store::Store::open(db_path, data_dir.join("cas"))?;
    let mut node = crate::node_sync::open_node(data_dir, &store)?;
    // The record must exist before it can be enabled: a strategy that was
    // never installed is installed here, with the compiled-in version.
    reconcile_units(&mut node, &data_dir.join("policy-units"))?;
    let outcome = set_enabled(&mut node, id, enabled)?;
    println!("{}", serde_json::to_string_pretty(&outcome)?);
    Ok(())
}

/// The committed window chain of the subject scope.
fn windows_command(data_dir: &Path, limit: u64) -> Result<()> {
    let db_path = data_dir.join("shenko.db");
    anyhow::ensure!(
        db_path.is_file(),
        "no managed installation at {}; run `shenko start` once",
        data_dir.display()
    );
    let store =
        std::sync::Arc::new(shenko_store::store::Store::open(db_path, data_dir.join("cas"))?);
    println!("{}", serde_json::to_string_pretty(&window_report(&store, limit)?)?);
    Ok(())
}
