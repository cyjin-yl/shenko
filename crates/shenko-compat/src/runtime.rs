//! Installation observation over the real Node and Cordis authorities. This
//! owns no ecosystem catalog, service registry or external implementation.
use std::{future::Future, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use cordis_core::{Context, FiberHandle, Plugin};
use parking_lot::Mutex;
use shenko_kernel::{Supervisor, plugin::PluginRegistry};
use shenko_node::Node;
use shenko_store::Store;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::installation::plugin_package;

#[derive(Clone)]
pub struct AdapterHost {
    pub store: Arc<Store>,
    pub node: Arc<Mutex<Node>>,
    /// The assembly owns one artifact reader for the adapters that need it.
    /// Its installation watcher must outlive each resident adapter generation.
    pub shared_artifact_reader: bool,
    registry: Arc<PluginRegistry>,
    shutdown: CancellationToken,
}

impl AdapterHost {
    pub fn new(supervisor: &Supervisor, store: Arc<Store>, node: Arc<Mutex<Node>>) -> Self {
        Self { store, node, shared_artifact_reader: false, registry: supervisor.plugins.clone(), shutdown: supervisor.lifecycle.shutdown_token() }
    }

    /// Opt in only when the assembly supplies the reader's shared lifecycle.
    pub fn with_shared_artifact_reader(mut self) -> Self {
        self.shared_artifact_reader = true;
        self
    }
}

/// Observe one independently installed adapter. Preparation is local; external
/// connections still belong to its native task attempts, never this watcher.
/// Node polling also observes writes made by another CLI process. The initial
/// reconciliation completes before publication returns to the caller.
pub async fn watch_installed<P, F, Fut>(ctx: Context, host: AdapterHost, unit_id: &'static str, plugin: P, maintain: F) -> Result<()>
where
    P: Plugin<Config = (Arc<Store>, Arc<Mutex<Node>>)> + Clone + Sync,
    F: Fn(Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send,
{
    watch_any_installed(ctx, host, &[unit_id], plugin, maintain).await
}

/// Own one generation while any listed exact adapter installation is enabled.
/// The IDs are copied before the watcher starts; the caller's slice need not
/// outlive this startup call. Each fact is checked even if another is enabled.
pub async fn watch_any_installed<P, F, Fut>(ctx: Context, host: AdapterHost, unit_ids: &[&'static str], plugin: P, maintain: F) -> Result<()>
where
    P: Plugin<Config = (Arc<Store>, Arc<Mutex<Node>>)> + Clone + Sync,
    F: Fn(Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send,
{
    anyhow::ensure!(!unit_ids.is_empty(), "compatibility watcher needs at least one installation");
    let unit_ids = unit_ids.to_vec();
    let expected = unit_ids.iter().map(|unit_id| plugin_package(unit_id).map(|package| package.unit))
        .collect::<Result<Vec<_>>>()?;
    let agent_id = host.store.get_existing_agent_id()?;
    let stop = CancellationToken::new();
    let worker_stop = stop.clone();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
    let (finished_tx, finished_rx) = oneshot::channel::<Result<()>>();
    let worker_ctx = ctx.clone();
    ctx.run(async move {
        let mut ready = Some(ready_tx);
        let mut resident: Option<FiberHandle> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let result: Result<()> = async {
            loop {
                tokio::select! { biased;
                    _ = worker_stop.cancelled() => return Ok(()),
                    _ = host.shutdown.cancelled() => return Ok(()),
                    _ = tick.tick() => {},
                }
                let enabled = {
                    let node = host.node.lock();
                    let mut enabled = false;
                    for expected in &expected {
                        let matching = node.installed_unit_fact(&expected.unit_id)?.is_some_and(|fact|
                            fact.enabled && fact.revision == expected.revision && fact.artifact_digest == expected.artifact_digest);
                        enabled |= matching;
                    }
                    enabled
                };
                if !enabled {
                    if let Some(fiber) = resident.take() {
                        host.registry.uninstall(&fiber).await?;
                    }
                } else {
                    if resident.is_none() {
                        resident = Some(host.registry.spawn(plugin.clone(), (host.store.clone(), host.node.clone())).await?);
                    }
                    maintain(worker_ctx.clone()).await?;
                }
                if let Some(ready) = ready.take() { let _ = ready.send(Ok(())); }
            }
        }.await;
        // Terminal cleanup uses the native Fiber barrier even after the host
        // has closed admission. The outer registry observes Cordis diagnostics
        // after this generation's own fallible cleanup has completed.
        let cleanup = match resident {
            Some(fiber) => fiber.dispose().await.map_err(anyhow::Error::from),
            None => Ok(()),
        };
        let result = match (result, cleanup) {
            (Err(error), Err(cleanup)) => Err(error.context(format!("adapter retirement also failed: {cleanup}"))),
            (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup,
        };
        if let Err(error) = &result {
            if let Some(agent_id) = agent_id {
                host.store.journal_runtime_error(agent_id, shenko_store::RuntimeErrorContext {
                    kind: "compatibility_lifecycle".into(), source_module: module_path!().into(),
                    message: format!("{} installation observer failed: {error:#}", unit_ids.join(", ")), step_no: None, task_id: None,
                    extra: serde_json::json!({"unit_ids":unit_ids}),
                });
            } else {
                tracing::error!(unit_ids = ?unit_ids, %error, "adapter lifecycle failed before subject initialization");
            }
        }
        if let Some(ready) = ready {
            let message = match &result { Err(error) => format!("{error:#}"), Ok(()) => "adapter stopped during startup".into() };
            let _ = ready.send(Err(anyhow::anyhow!(message)));
        }
        let _ = finished_tx.send(result);
    })?;
    // ctx.run commits task ownership before it starts. Register cancellation
    // after the join effect, so LIFO disposal signals stop before joining it.
    let cleanup_stop = stop.clone();
    if let Err(error) = ctx.effect(move || async move {
        cleanup_stop.cancel();
        finished_rx.await.context("adapter installation observer lost its outcome").map_err(std::io::Error::other)?
            .map_err(std::io::Error::other)
    }) {
        stop.cancel();
        return Err(error.into());
    }
    ready_rx.await.context("adapter installation observer lost startup")?
}
