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
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64_STD};
use cordis_core::{Context, FiberHandle, Plugin};
use parking_lot::Mutex;
use serde_json::{Value, json};
use shenko_context_policy::{
    ContextPolicy, NotesJournal, PolicyManifest, PolicyRegistry,
};
use shenko_kernel::Supervisor;
use shenko_kernel::plugin::PluginRegistry;
use shenko_node::{
    Node, UnitDescriptor, UnitExport, UnitKind, UnitPackage, UnitRequest, UNIT_SCHEMA,
};
use shenko_store::Store;
use shenko_subject::NativeTaskHandlerPlugin;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
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
    /// The manifest facts of the compiled-in implementation. This is the
    /// artifact identity the installation record must name.
    pub manifest: fn() -> PolicyManifest,
}

/// Every built-in context strategy.
pub fn catalog() -> Vec<PolicyEntry> {
    vec![
        PolicyEntry {
            id: shenko_compact_budget::POLICY_ID,
            default_config: shenko_compact_budget::BudgetCompact::default_config(),
            manifest: || {
                shenko_compact_budget::BudgetCompact::new(Default::default()).manifest()
            },
        },
        PolicyEntry {
            id: shenko_compact_snap::POLICY_ID,
            default_config: shenko_compact_snap::SnapCompact::default_config(),
            manifest: || {
                shenko_compact_snap::SnapCompact::new(Default::default()).manifest()
            },
        },
        PolicyEntry {
            id: shenko_compact_rollover::POLICY_ID,
            default_config: shenko_compact_rollover::NotesRollover::default_config(),
            manifest: shenko_compact_rollover::NotesRollover::manifest_document,
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
    Rollover(shenko_compact_rollover::RolloverConfig),
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
        let manifest = (entry.manifest)();
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
        import_record(node, &policy, &policy.config, enabled, package_root)?;
    }
    installed_policies(node)
}

/// Import one compiled strategy's installation record at the manifest
/// identity, with the given enable state and configuration document.
///
/// The artifact identity is the compiled manifest, never the caller's
/// configuration: a reconfiguration changes the record's config while the
/// implementation it names stays the same.
fn import_record(
    node: &mut Node,
    policy: &InstalledPolicy,
    config: &Value,
    enabled: bool,
    package_root: &Path,
) -> Result<()> {
    let manifest = &policy.manifest;
    let artifact = serde_json::to_vec(&manifest_document(manifest))?;
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
                artifact_digest: digest,
                provides: manifest.provides.clone(),
                dependencies: Vec::new(),
                config: config.clone(),
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
    Ok(())
}

/// Install or disable one strategy's installation record.
pub fn set_enabled(node: &mut Node, policy_id: &str, enabled: bool) -> Result<Value> {
    let unit = unit_id(policy_id);
    node.handle_unit_request(UnitRequest::SetEnabled { unit_id: unit, enabled })
}

/// Replace one strategy's configuration at the same compiled identity.
///
/// This is the authored reconfiguration path: the record keeps naming the
/// compiled implementation and the runtime observer must notice the new
/// document and reinstall the strategy with it.
pub fn set_config(
    node: &mut Node,
    policy_id: &str,
    config: Value,
    package_root: &Path,
) -> Result<Value> {
    let installed = installed_policies(node)?;
    let policy = installed
        .iter()
        .find(|policy| policy.id == policy_id)
        .with_context(|| format!("context strategy {policy_id} has no installation record"))?;
    import_record(node, policy, &config, policy.enabled, package_root)?;
    Ok(manifest_document(&policy.manifest))
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
    runtime_from_config(&policy.id, &policy.config)
}

/// The runtime configuration of any strategy id, read from its installation
/// record. A malformed configuration is an error, never a silently ignored
/// default.
pub fn runtime_from_config(id: &str, config: &Value) -> Result<PolicyRuntime, String> {
    match id {
        shenko_compact_budget::POLICY_ID => Ok(PolicyRuntime::Budget(
            shenko_compact_budget::BudgetCompact::config_from_json(config)?,
        )),
        shenko_compact_snap::POLICY_ID => Ok(PolicyRuntime::Snap(
            shenko_compact_snap::SnapCompact::config_from_json(config)?,
        )),
        shenko_compact_rollover::POLICY_ID => Ok(PolicyRuntime::Rollover(
            shenko_compact_rollover::NotesRollover::config_from_json(config)?,
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
        let manifest = (entry.manifest)();
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
            // The notebook entry is working state, so the report names the
            // revision and provenance the window carries, never the text.
            let mut notes = Vec::new();
            for entry in &meta.window.entries {
                let kind = match entry {
                    shenko_context_policy::WindowEntry::Verbatim { .. } => "verbatim",
                    shenko_context_policy::WindowEntry::Pointer { .. } => "pointer",
                    shenko_context_policy::WindowEntry::Text { .. } => "text",
                    shenko_context_policy::WindowEntry::Image { .. } => "image",
                    shenko_context_policy::WindowEntry::Note { .. } => "note",
                };
                let count = kinds.entry(kind.to_string()).or_insert(json!(0));
                *count = json!(count.as_u64().unwrap_or(0) + 1);
                if let shenko_context_policy::WindowEntry::Note { revision, source, .. } = entry {
                    notes.push(json!({"revision": revision, "source": source}));
                }
            }
            json!({
                "id": meta.window.id.to_string(),
                "generation": meta.window.generation,
                "policy_id": meta.window.policy_id.clone(),
                "policy_version": meta.window.policy_version.clone(),
                "model_generation": meta.window.model_generation,
                "covers_through": meta.window.covers_through,
                "entry_kinds": Value::Object(kinds),
                "notes": notes,
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
        ContextPolicyCommand::Config { id, config } => set_policy_config(data_dir, &id, &config),
    }
}

/// Replace one strategy's configuration document, keeping the compiled
/// identity and the record's enable state.
///
/// A running process observes the record and installs the new configuration
/// at its next tick; no restart is required, and a window prepared under the
/// previous configuration can no longer commit.
fn set_policy_config(data_dir: &Path, id: &str, config: &str) -> Result<()> {
    let known = catalog().iter().any(|entry| entry.id == id);
    anyhow::ensure!(
        known,
        "unknown context strategy {id}; run `shenko context policy list` for the installable ones"
    );
    let document: Value = serde_json::from_str(config)
        .with_context(|| format!("context strategy {id} configuration is not JSON"))?;
    let db_path = data_dir.join("shenko.db");
    anyhow::ensure!(
        db_path.is_file(),
        "no managed installation at {}; run `shenko start` once",
        data_dir.display()
    );
    let store = shenko_store::store::Store::open(db_path, data_dir.join("cas"))?;
    let mut node = crate::node_sync::open_node(data_dir, &store)?;
    let package_root = data_dir.join("policy-units");
    reconcile_units(&mut node, &package_root)?;
    // Parse before writing, exactly as the runtime will: a document the
    // installed implementation cannot read never becomes the record.
    runtime_from_config(id, &document).map_err(anyhow::Error::msg)?;
    let manifest = set_config(&mut node, id, document, &package_root)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "strategy": id,
            "artifact_digest": manifest.get("version"),
            "artifact_identity": manifest,
            "note": "a running process installs the new configuration at its next observation; a window prepared under the previous configuration can no longer commit",
        }))?
    );
    Ok(())
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

/// The services every installed strategy runs against.
///
/// The registry and the notebook journal are process services: the unit's
/// JSON configuration never names them, so a changed configuration document
/// cannot redirect a strategy at another branch's notes or another chain.
#[derive(Clone)]
pub struct PolicyHosts {
    pub registry: Arc<PolicyRegistry>,
    pub notes: Arc<dyn NotesJournal>,
    pub store: Arc<Store>,
    pub node: Arc<Mutex<Node>>,
}

struct Resident {
    policy: FiberHandle,
    /// The installed native definition that belongs to this strategy unit, if
    /// any. It is withdrawn with the policy, never left behind.
    tool: Option<FiberHandle>,
}

#[derive(Clone)]
struct PolicyWatch {
    unit_id: String,
    manifest: PolicyManifest,
    expected_digest: Digest,
    hosts: PolicyHosts,
    plugins: Arc<PluginRegistry>,
    shutdown: CancellationToken,
}

/// Observe every compiled context strategy against the Node installation
/// record for the lifetime of this runtime.
///
/// This is the live projection, not a boot snapshot: a unit enabled, disabled,
/// replaced or reconfigured by another Node connection changes the resident
/// Fiber set without restarting the CLI. The initial reconciliation completes
/// before this call returns, so the caller never reports readiness from a
/// half-installed strategy set.
pub async fn spawn_installed_policies(
    supervisor: &Supervisor,
    hosts: PolicyHosts,
) -> Result<Vec<FiberHandle>> {
    let mut handles = Vec::new();
    for entry in catalog() {
        let manifest = (entry.manifest)();
        let expected_digest = Digest::blake3(&serde_json::to_vec(&manifest_document(&manifest))?);
        handles.push(
            supervisor
                .spawn(
                    InstalledPolicyObserver,
                    PolicyWatch {
                        unit_id: unit_id(entry.id),
                        manifest,
                        expected_digest,
                        hosts: hosts.clone(),
                        plugins: supervisor.plugins.clone(),
                        shutdown: supervisor.lifecycle.shutdown_token(),
                    },
                )
                .await?,
        );
        tracing::info!(unit_id = entry.id, "context strategy installation observer started");
    }
    Ok(handles)
}

struct InstalledPolicyObserver;

impl Plugin for InstalledPolicyObserver {
    type Config = PolicyWatch;
    type Input = PolicyWatch;
    type PrepareError = std::convert::Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("context-policy-installation-observer")
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, watch: &Self::Input) -> Result<(), Self::ApplyError> {
        watch_installed_policy(ctx, watch.clone()).await.map_err(std::io::Error::other)
    }
}

/// Poll the one installed-unit fact this strategy depends on and reconcile the
/// resident Fiber set. Node polling observes writes made by another CLI
/// connection, which the in-process hook pump cannot see.
async fn watch_installed_policy(ctx: Context, watch: PolicyWatch) -> Result<()> {
    let agent_id = watch.hosts.store.get_existing_agent_id()?;
    let stop = CancellationToken::new();
    let worker_stop = stop.clone();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
    let (finished_tx, finished_rx) = oneshot::channel::<Result<()>>();
    let worker_ctx = ctx.clone();
    ctx.run(async move {
        let mut ready = Some(ready_tx);
        // The exact configuration document the resident was started from.
        // Artifact identity alone is not enough: the Node permits a same-
        // identity record whose config was replaced, and that is a real
        // configuration change.
        let mut resident: Option<(Value, Resident)> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let result: Result<()> = async {
            loop {
                tokio::select! { biased;
                    _ = worker_stop.cancelled() => return Ok(()),
                    _ = watch.shutdown.cancelled() => return Ok(()),
                    _ = tick.tick() => {},
                }
                let fact = watch.hosts.node.lock().installed_unit_fact(&watch.unit_id)?;
                let current = fact.filter(|fact| {
                    fact.enabled
                        && fact.revision == watch.manifest.version
                        && fact.artifact_digest == watch.expected_digest
                });
                match current {
                    None => retire(&watch, &mut resident).await?,
                    Some(fact) => {
                        let config = fact.descriptor.config;
                        if resident.as_ref().is_none_or(|(applied, _)| applied != &config) {
                            retire(&watch, &mut resident).await?;
                            let runtime = runtime_from_config(&watch.manifest.id, &config)
                                .map_err(|error| anyhow::anyhow!(
                                    "{} installation record carries an unusable configuration: {error}",
                                    watch.unit_id
                                ))?;
                            resident = Some((config, spawn_resident(&watch, runtime).await?));
                        }
                    }
                }
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Ok(()));
                }
            }
        }
        .await;
        // Terminal cleanup uses the native Fiber barrier even after the host
        // has closed admission, and keeps the primary error.
        let cleanup = retire(&watch, &mut resident).await;
        let result = match (result, cleanup) {
            (Err(error), Err(cleanup)) => {
                Err(error.context(format!("context strategy retirement also failed: {cleanup}")))
            }
            (Err(error), _) => Err(error),
            (Ok(()), cleanup) => cleanup,
        };
        if let Err(error) = &result {
            journal_policy_failure(
                &watch,
                agent_id,
                format!("{} installation observer failed: {error:#}", watch.unit_id),
            );
        }
        if let Some(ready) = ready {
            let message = match &result {
                Err(error) => format!("{error:#}"),
                Ok(()) => "context strategy observer stopped during startup".into(),
            };
            let _ = ready.send(Err(anyhow::anyhow!(message)));
        }
        let _ = finished_tx.send(result);
    })?;
    // ctx.run commits task ownership before it starts. Register cancellation
    // after the join effect, so LIFO disposal signals stop before joining it.
    let cleanup_stop = stop.clone();
    if let Err(error) = ctx.effect(move || async move {
        cleanup_stop.cancel();
        finished_rx
            .await
            .context("context strategy observer lost its outcome")
            .map_err(std::io::Error::other)?
            .map_err(std::io::Error::other)
    }) {
        stop.cancel();
        return Err(error.into());
    }
    ready_rx.await.context("context strategy observer lost startup")?
}

async fn retire(watch: &PolicyWatch, resident: &mut Option<(Value, Resident)>) -> Result<()> {
    let Some((_, resident)) = resident.take() else {
        return Ok(());
    };
    // The installed native definition goes first: a withdrawn policy must not
    // leave tools that can still write notes or request windows.
    let mut first: Option<anyhow::Error> = None;
    if let Some(tool) = resident.tool {
        if let Err(error) = watch.plugins.uninstall(&tool).await {
            first = Some(error);
        }
    }
    if let Err(error) = watch.plugins.uninstall(&resident.policy).await {
        first = Some(match first {
            Some(previous) => previous.context(format!(
                "native definition retirement also failed: {error}"
            )),
            None => error,
        });
    }
    match first {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn spawn_resident(watch: &PolicyWatch, runtime: PolicyRuntime) -> Result<Resident> {
    match runtime {
        PolicyRuntime::Budget(config) => Ok(Resident {
            policy: watch
                .plugins
                .spawn(shenko_compact_budget::BudgetCompactPlugin, config)
                .await?,
            tool: None,
        }),
        PolicyRuntime::Snap(config) => Ok(Resident {
            policy: watch
                .plugins
                .spawn(shenko_compact_snap::SnapCompactPlugin, config)
                .await?,
            tool: None,
        }),
        PolicyRuntime::Rollover(config) => {
            let policy = watch
                .plugins
                .spawn(
                    shenko_compact_rollover::NotesRolloverPlugin,
                    shenko_compact_rollover::RolloverHosts {
                        config,
                        notes: Arc::clone(&watch.hosts.notes),
                        registry: Arc::clone(&watch.hosts.registry),
                    },
                )
                .await?;
            let handler: Arc<dyn shenko_subject::NativeTaskHandler> =
                Arc::new(shenko_compact_rollover::ContextNotebookHandler::new(
                    Arc::clone(&watch.hosts.store),
                    Arc::clone(&watch.hosts.notes),
                    Arc::clone(&watch.hosts.registry),
                ));
            match watch.plugins.spawn(NativeTaskHandlerPlugin, handler).await {
                Ok(tool) => Ok(Resident { policy, tool: Some(tool) }),
                Err(error) => {
                    Err(match watch.plugins.uninstall(&policy).await {
                        Ok(()) => error,
                        Err(cleanup) => error
                            .context(format!("strategy retirement after tool failure also failed: {cleanup}")),
                    })
                }
            }
        }
    }
}

fn journal_policy_failure(watch: &PolicyWatch, agent_id: Option<EntityId>, message: String) {
    match agent_id {
        Some(agent_id) => {
            let context = shenko_store::RuntimeErrorContext {
                kind: "context_policy_lifecycle".into(),
                source_module: module_path!().into(),
                message,
                step_no: None,
                task_id: None,
                extra: json!({"unit_id": watch.unit_id}),
            };
            // Journals a runtime-error fact and counts any dropped write
            // itself; a lifecycle failure must not be hidden by a second
            // failure here.
            watch.hosts.store.journal_runtime_error(agent_id, context);
        }
        None => tracing::error!(
            unit_id = %watch.unit_id,
            message = %message,
            "context policy lifecycle failed before subject initialization"
        ),
    }
}

#[cfg(test)]
mod installed_policy_tests {
    //! The live projection, against a real Node, Store and Cordis supervisor:
    //! a strategy starts, changes configuration, stops and restarts because the
    //! installation record changed on another connection — never because the
    //! process was restarted.
    use super::*;
    use shenko_types::ids::RuntimeGeneration;

    fn fixture(name: &str) -> (std::path::PathBuf, Arc<Store>, Node) {
        let dir = std::env::temp_dir().join(format!("shenko-{name}-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        let mut node = Node::open(&dir.join("node")).unwrap();
        node.handle(shenko_node::Request::Initialize {
            node_id: EntityId::new(),
            identity: None,
        })
        .unwrap();
        (dir, store, node)
    }

    fn installed_epoch(registry: &PolicyRegistry, id: &str) -> Option<u64> {
        registry
            .installed()
            .into_iter()
            .find(|(manifest, _)| manifest.id == id)
            .map(|(_, epoch)| epoch)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_unit_change_on_another_connection_installs_replaces_and_stops_the_strategy() {
        let (dir, store, mut writer) = fixture("policy-observer");
        let package_root = dir.join("policy-units");
        let records = reconcile_units(&mut writer, &package_root).unwrap();
        // Start with every strategy disabled: a disabled record installs
        // nothing, so the first readiness is genuinely "nothing installed".
        for policy in &records {
            set_enabled(&mut writer, &policy.id, false).unwrap();
        }
        let scope = EntityId::new();
        let registry = Arc::new(PolicyRegistry::with_ledger(shenko_context_policy::WindowLedger::new(Arc::new(
            shenko_context_policy::StoreWindowLog::new(Arc::clone(&store), scope),
        ))));
        let notes: Arc<dyn NotesJournal> =
            Arc::new(shenko_context_policy::StoreNotesJournal::new(Arc::clone(&store), scope));
        // The observer reads the installation records through its own Node
        // connection, exactly as it must in production.
        let observer_node = Arc::new(Mutex::new(Node::open(&dir.join("node")).unwrap()));

        let supervisor = Supervisor::new(RuntimeGeneration::default());
        supervisor
            .spawn(shenko_context_policy::ContextPolicyHost, Arc::clone(&registry))
            .await
            .unwrap();
        supervisor
            .spawn(
                shenko_subject::NativeChildPlugin,
                (Arc::clone(&store), RuntimeGeneration::default()),
            )
            .await
            .unwrap();
        let _observers = spawn_installed_policies(
            &supervisor,
            PolicyHosts {
                registry: Arc::clone(&registry),
                notes,
                store: Arc::clone(&store),
                node: observer_node,
            },
        )
        .await
        .unwrap();
        assert!(registry.installed().is_empty(), "a disabled unit installs nothing");

        // Enable the notes rollover unit on the other connection.
        set_enabled(&mut writer, shenko_compact_rollover::POLICY_ID, true).unwrap();
        wait_for(|| installed_epoch(&registry, shenko_compact_rollover::POLICY_ID).is_some()).await;
        let first = installed_epoch(&registry, shenko_compact_rollover::POLICY_ID).unwrap();

        // Reconfigure it at the same artifact identity: the threshold the
        // strategy reports must be the new document's, and the epoch advances
        // so a window prepared under the old configuration cannot commit.
        set_config(
            &mut writer,
            shenko_compact_rollover::POLICY_ID,
            json!({"trigger_ratio": 0.5, "reminder_ratio": 0.6, "retain_units": 4}),
            &package_root,
        )
        .unwrap();
        let caps = shenko_context_policy::ModelCaps { model_id: "fixture".into(), max_context: 1000, vision: None };
        wait_for(|| {
            let epoch = installed_epoch(&registry, shenko_compact_rollover::POLICY_ID);
            epoch.is_some_and(|epoch| epoch > first) && registry.should_compact(&caps, 600)
        })
        .await;
        let second = installed_epoch(&registry, shenko_compact_rollover::POLICY_ID).unwrap();
        assert!(second > first, "a reconfiguration installs a new epoch");

        // An independent strategy keeps running while this one stops.
        set_enabled(&mut writer, shenko_compact_budget::POLICY_ID, true).unwrap();
        wait_for(|| installed_epoch(&registry, shenko_compact_budget::POLICY_ID).is_some()).await;
        set_enabled(&mut writer, shenko_compact_rollover::POLICY_ID, false).unwrap();
        wait_for(|| installed_epoch(&registry, shenko_compact_rollover::POLICY_ID).is_none()).await;
        assert!(
            installed_epoch(&registry, shenko_compact_budget::POLICY_ID).is_some(),
            "disabling one strategy must not stop another"
        );
        assert!(!registry.should_compact(&caps, 600));

        // Re-enabling installs a fresh epoch above the retired one.
        set_enabled(&mut writer, shenko_compact_rollover::POLICY_ID, true).unwrap();
        wait_for(|| {
            installed_epoch(&registry, shenko_compact_rollover::POLICY_ID).is_some_and(|epoch| epoch > second)
        })
        .await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_implementation_mismatch_installs_nothing() {
        let (dir, store, mut writer) = fixture("policy-mismatch");
        let package_root = dir.join("policy-units");
        reconcile_units(&mut writer, &package_root).unwrap();
        // Rewrite one record at a revision this build does not implement.
        let mut records = installed_policies(&writer).unwrap();
        let record = records
            .iter_mut()
            .find(|policy| policy.id == shenko_compact_budget::POLICY_ID)
            .unwrap();
        record.manifest.version = "999.0.0".into();
        import_record(&mut writer, record, &record.config.clone(), true, &package_root).unwrap();

        let scope = EntityId::new();
        let registry = Arc::new(PolicyRegistry::with_ledger(shenko_context_policy::WindowLedger::new(Arc::new(
            shenko_context_policy::StoreWindowLog::new(Arc::clone(&store), scope),
        ))));
        let notes: Arc<dyn NotesJournal> =
            Arc::new(shenko_context_policy::StoreNotesJournal::new(Arc::clone(&store), scope));
        let supervisor = Supervisor::new(RuntimeGeneration::default());
        supervisor
            .spawn(shenko_context_policy::ContextPolicyHost, Arc::clone(&registry))
            .await
            .unwrap();
        supervisor
            .spawn(
                shenko_subject::NativeChildPlugin,
                (Arc::clone(&store), RuntimeGeneration::default()),
            )
            .await
            .unwrap();
        let _observers = spawn_installed_policies(
            &supervisor,
            PolicyHosts {
                registry: Arc::clone(&registry),
                notes,
                store: Arc::clone(&store),
                node: Arc::new(Mutex::new(Node::open(&dir.join("node")).unwrap())),
            },
        )
        .await
        .unwrap();
        assert!(
            installed_epoch(&registry, shenko_compact_budget::POLICY_ID).is_none(),
            "an installed revision this build does not implement is not relabelled as the compiled one"
        );
        // The other strategies still install: one mismatch retires one unit.
        wait_for(|| installed_epoch(&registry, shenko_compact_snap::POLICY_ID).is_some()).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Poll a condition with a bounded deadline; the observer reconciles on its
    /// own 250 ms tick, so the test never reaches into it.
    async fn wait_for(mut condition: impl FnMut() -> bool) {
        for _ in 0..40 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the live projection did not converge");
    }
}
