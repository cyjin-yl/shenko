//! Recurring passive observations of the installed cognition catalogue.
//! Acquisition and archival belong to one retained preparation worker; S0
//! only selects already prepared facts for its independently read revision.
use std::{sync::Arc, time::Duration};

use anyhow::Result;
use shenko_awareness_core::{
    CognitionCatalog, InstallationFactsSource, INSTALLATION_UNITS, PerceptionBinding,
    PerceptionSourceId,
};
use shenko_kernel::preparation::{ContextPreparation, PreparationCancellation};
use shenko_types::ids::EntityId;
use tokio::sync::oneshot;

// External Node writers have no local change notification. Skip missed ticks
// and keep a single original blocking worker, never overlapping archive batches.
const OBSERVATION_REFRESH: Duration = Duration::from_millis(250);

/// Exact registrations created by this worker, never another owner's source.
struct InstallationSourceRegistrations {
    catalog: Arc<CognitionCatalog>,
    installed: Option<(PerceptionBinding, EntityId)>,
}
impl InstallationSourceRegistrations {
    fn reconcile(&mut self, facts: &[shenko_node::InstalledUnitFact]) -> Result<()> {
        let wanted = facts.iter().find(|fact| fact.enabled
            && fact.kind == shenko_node::UnitKind::Perception
            && fact.unit_id == INSTALLATION_UNITS).map(PerceptionBinding::of);
        if self.installed.as_ref().map(|(binding, _)| binding) == wanted.as_ref() {
            return Ok(());
        }
        if let Some((_, id)) = self.installed.take() {
            self.catalog.perception().unbind(id);
        }
        if let Some(binding) = wanted {
            let source = Arc::new(InstallationFactsSource::new(
                Arc::clone(self.catalog.node()), binding.clone())?);
            // Strict collision admission: a duplicate owner is not silently
            // adopted, and retirement can only withdraw the ID created here.
            let id = self.catalog.perception().bind_source(
                binding.clone(), PerceptionSourceId::builtin(), source)?;
            self.installed = Some((binding, id));
        }
        Ok(())
    }
}
impl Drop for InstallationSourceRegistrations {
    fn drop(&mut self) {
        if let Some((_, id)) = self.installed.take() {
            self.catalog.perception().unbind(id);
        }
    }
}

pub struct RuntimePerceptionPlugin;
impl cordis_core::Plugin for RuntimePerceptionPlugin {
    type Config = Arc<CognitionCatalog>;
    type Input = Self::Config;
    type PrepareError = std::convert::Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> std::borrow::Cow<'_, str> { "runtime-perception-sources".into() }
    fn prepare(&self, catalog: Self::Config) -> std::result::Result<Self::Input, Self::PrepareError> {
        Ok(catalog)
    }
    async fn apply(&self, ctx: cordis_core::Context, catalog: &Self::Input)
        -> std::result::Result<(), Self::ApplyError> {
        // Attach and reserve before capturing dependencies. Both original
        // generation teardown and kernel pre-stop join this same worker.
        let owner = ContextPreparation::<()>::attach(&ctx).await
            .map_err(std::io::Error::other)?;
        let permit = owner.reserve_start().map_err(std::io::Error::other)?;
        let catalog = Arc::clone(catalog);
        let runtime = tokio::runtime::Handle::current();
        let (ready_tx, ready_rx) = oneshot::channel();
        permit.start(move |cancel| {
            let result = observe_runtime(catalog, runtime, &cancel, ready_tx);
            if let Err(error) = &result {
                tracing::error!(owner = %cancel.owner_id(), %error,
                    "perception preparation owner failed");
            }
            result
        }).map_err(std::io::Error::other)?;
        ready_rx.await.map_err(std::io::Error::other)?
            .map_err(std::io::Error::other)
    }
}

fn prepare_observation(
    sources: &mut InstallationSourceRegistrations,
    cancel: &PreparationCancellation,
) -> Result<()> {
    cancel.checkpoint()?;
    let facts = sources.catalog.node().lock().installed_unit_facts()?;
    cancel.checkpoint()?;
    sources.reconcile(&facts)?;
    if let Some((binding, _)) = &sources.installed {
        cancel.checkpoint()?;
        // Failed acquisition is cached as Unavailable, never replaced by an
        // earlier healthy result. An unprepared exact revision stays Unknown.
        sources.catalog.perception().observe(binding);
        // An admitted archive can finish after replacement/disable. Retain its
        // old-version history while withdrawing only this owner's old cache.
        let current = sources.catalog.node().lock().installed_unit_facts()?;
        sources.reconcile(&current)?;
    }
    Ok(())
}

fn observe_runtime(
    catalog: Arc<CognitionCatalog>,
    runtime: tokio::runtime::Handle,
    cancel: &PreparationCancellation,
    ready: oneshot::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let mut sources = InstallationSourceRegistrations { catalog, installed: None };
    let initial = prepare_observation(&mut sources, cancel);
    let _ = ready.send(initial.as_ref().map(|_| ()).map_err(|error| format!("{error:#}")));
    initial?;
    runtime.block_on(async {
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + OBSERVATION_REFRESH, OBSERVATION_REFRESH);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { biased;
                _ = cancel.cancelled() => break,
                _ = tick.tick() => {},
            }
            if cancel.is_cancelled() { break; }
            prepare_observation(&mut sources, cancel)?;
        }
        Ok::<(), anyhow::Error>(())
    })?;
    // The retained worker drops its exact IDs after its last admitted archive.
    drop(sources);
    Ok(())
}
