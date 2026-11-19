//! Optional document plugin assembly. Node owns installation; Cordis owns the
//! resident generation. Nothing is installed or contacted merely by starting.
use std::{borrow::Cow, convert::Infallible, sync::Arc, time::Duration};
use anyhow::{Context as _, Result};
use cordis_core::{Context, FiberHandle, InjectSpec, Plugin, Service};
use parking_lot::Mutex;
use shenko_exec_host::{DocumentParseHandler, DOCUMENT_OXR_UNIT};
use shenko_kernel::{Supervisor, plugin::PluginRegistry, task::NativeTaskHandler};
use shenko_node::Node;
use shenko_store::Store;
use shenko_subject::{NativeChildService, NativeTaskHandlerPlugin};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Host { store: Arc<Store>, node: Arc<Mutex<Node>>, plugins: Arc<PluginRegistry>, shutdown: CancellationToken }

pub async fn install(supervisor: &Supervisor, store: Arc<Store>, node: Node) -> Result<()> {
    supervisor.spawn(Installations, Host { store, node: Arc::new(Mutex::new(node)),
        plugins: supervisor.plugins.clone(), shutdown: supervisor.lifecycle.shutdown_token() }).await?;
    Ok(())
}

struct Installations;
impl Plugin for Installations {
    type Config = Host;
    type Input = Host;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> Cow<'_, str> { Cow::Borrowed("shenko-document-installation") }
    fn inject(&self) -> InjectSpec { InjectSpec::none().require(NativeChildService::NAME) }
    fn prepare(&self, config: Host) -> Result<Host, Infallible> { Ok(config) }
    async fn apply(&self, ctx: Context, host: &Host) -> Result<(), std::io::Error> {
        let host = host.clone();
        let stop = CancellationToken::new();
        let worker_stop = stop.clone();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
        let (finished_tx, finished_rx) = oneshot::channel::<Result<()>>();
        ctx.run(async move {
            let mut ready = Some(ready_tx);
            let mut resident: Option<(String, FiberHandle)> = None;
            let mut rejected: Option<String> = None;
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let result: Result<()> = async {
                loop {
                    tokio::select! { biased;
                        _ = worker_stop.cancelled() => return Ok(()),
                        _ = host.shutdown.cancelled() => return Ok(()),
                        _ = tick.tick() => {},
                    }
                    let wanted = DocumentParseHandler::installed_identity(&host.node.lock())?;
                    if wanted.is_none() { rejected = None; }
                    if resident.as_ref().map(|(identity, _)| identity.as_str()) != wanted.as_deref() {
                        if let Some((_, fiber)) = resident.take() { host.plugins.uninstall(&fiber).await?; }
                        if wanted != rejected {
                            match crate::document_runtime::configured_handler(host.store.clone(), host.node.clone()) {
                                Ok(Some(handler)) => {
                                    let identity = handler.installation_identity()?;
                                    let fiber = host.plugins.spawn(Generation, handler).await?;
                                    resident = Some((identity, fiber)); rejected = None;
                                }
                                Ok(None) => { rejected = None; }
                                Err(error) => {
                                    // A malformed optional installation is a reported
                                    // unavailable capability, not a reason to stop the subject.
                                    if let Some(agent_id) = host.store.get_existing_agent_id()? {
                                        host.store.journal_runtime_error(agent_id, shenko_store::RuntimeErrorContext {
                                            kind: "document_provider_unavailable".into(), source_module: module_path!().into(),
                                            message: format!("{DOCUMENT_OXR_UNIT} installation unavailable: {error:#}"),
                                            step_no: None, task_id: None, extra: serde_json::json!({"unit_id":DOCUMENT_OXR_UNIT}),
                                        });
                                    } else { tracing::error!(%error, "OXR installation unavailable before subject initialization"); }
                                    rejected = wanted;
                                }
                            }
                        }
                    }
                    if let Some(ready) = ready.take() { let _ = ready.send(Ok(())); }
                }
            }.await;
            let cleanup = match resident { Some((_, fiber)) => fiber.dispose().await.map_err(anyhow::Error::from), None => Ok(()) };
            let result = match (result, cleanup) {
                (Err(error), Err(cleanup)) => Err(error.context(format!("document retirement also failed: {cleanup}"))),
                (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup,
            };
            if let Err(error) = &result {
                tracing::error!(%error, "document installation observer failed; owned definitions withdrawn");
                if let Ok(Some(agent_id)) = host.store.get_existing_agent_id() {
                    host.store.journal_runtime_error(agent_id, shenko_store::RuntimeErrorContext {
                        kind: "document_lifecycle".into(), source_module: module_path!().into(),
                        message: format!("document installation observer failed: {error:#}"), step_no: None,
                        task_id: None, extra: serde_json::json!({"unit_id":DOCUMENT_OXR_UNIT}),
                    });
                }
            }
            if let Some(ready) = ready {
                let message = result.as_ref().err().map(|error| format!("{error:#}"))
                    .unwrap_or_else(|| "document installation observer stopped during startup".into());
                let _ = ready.send(Err(anyhow::anyhow!(message)));
            }
            let _ = finished_tx.send(result);
        }).map_err(std::io::Error::other)?;
        let cleanup_stop = stop.clone();
        if let Err(error) = ctx.effect(move || async move {
            cleanup_stop.cancel();
            finished_rx.await.context("document installation observer lost its terminal outcome")
                .map_err(std::io::Error::other)?.map_err(std::io::Error::other)
        }) { stop.cancel(); return Err(std::io::Error::other(error)); }
        ready_rx.await.context("document installation observer lost startup")
            .map_err(std::io::Error::other)?.map_err(std::io::Error::other)
    }
}

pub(crate) struct Generation;
impl Plugin for Generation {
    type Config = DocumentParseHandler;
    type Input = DocumentParseHandler;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> Cow<'_, str> { Cow::Borrowed("shenko-document-generation") }
    fn inject(&self) -> InjectSpec { InjectSpec::none().require(NativeChildService::NAME) }
    fn prepare(&self, handler: Self::Config) -> Result<Self::Input, Infallible> { Ok(handler) }
    async fn apply(&self, ctx: Context, handler: &DocumentParseHandler) -> Result<(), std::io::Error> {
        let cleanup = handler.clone();
        // LIFO disposal withdraws the definitions and recovery reader before joining.
        ctx.effect(move || async move { cleanup.shutdown().await; Ok::<_, std::io::Error>(()) })
            .map_err(std::io::Error::other)?;
        let parser: Arc<dyn NativeTaskHandler> = Arc::new(handler.clone());
        NativeTaskHandlerPlugin.apply(ctx.clone(), &parser).await?;
        let reader: Arc<dyn NativeTaskHandler> = Arc::new(handler.page_reader());
        NativeTaskHandlerPlugin.apply(ctx.clone(), &reader).await?;
        NativeTaskHandlerPlugin::install_recovery(ctx, handler.legacy_recovery_handler())
    }
}
