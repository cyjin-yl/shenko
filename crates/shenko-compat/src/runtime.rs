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
    registry: Arc<PluginRegistry>,
    shutdown: CancellationToken,
}

impl AdapterHost {
    pub fn new(supervisor: &Supervisor, store: Arc<Store>, node: Arc<Mutex<Node>>) -> Self {
        Self { store, node, registry: supervisor.plugins.clone(), shutdown: supervisor.lifecycle.shutdown_token() }
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
    let expected = plugin_package(unit_id)?.unit;
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
                let enabled = host.node.lock().installed_unit_fact(unit_id)?.is_some_and(|fact|
                    fact.enabled && fact.revision == expected.revision && fact.artifact_digest == expected.artifact_digest);
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
                    message: format!("{unit_id} installation observer failed: {error:#}"), step_no: None, task_id: None,
                    extra: serde_json::json!({"unit_id":unit_id}),
                });
            } else {
                tracing::error!(unit_id, %error, "adapter lifecycle failed before subject initialization");
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
