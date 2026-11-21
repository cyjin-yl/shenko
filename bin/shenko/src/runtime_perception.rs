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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use parking_lot::Mutex;
    use shenko_awareness_core::{ArchivedPerceptionSource, PerceptionFact,
        PerceptionObserver, PerceptionPerspective};
    use anyhow::ensure;
    use std::sync::{Condvar, atomic::{AtomicBool, AtomicUsize, Ordering}};
    use shenko_awareness_core::PerceptionStatus;
    use shenko_types::ids::RuntimeGeneration;
    use tokio::sync::Notify;

    struct SourceGate {
        armed: AtomicBool,
        calls: AtomicUsize,
        entered: Notify,
        released: std::sync::Mutex<bool>,
        changed: Condvar,
    }
    impl SourceGate {
        fn release(&self) {
            *self.released.lock().expect("source gate") = true;
            self.changed.notify_all();
        }
        fn read(&self) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                let released = self.released.lock().expect("source gate");
                let (released, _) = self.changed.wait_timeout_while(
                    released, Duration::from_secs(10), |released| !*released)
                    .expect("source gate wait");
                ensure!(*released, "original source gate was not released");
            }
            Ok("native owner consumer: unchanged local source".into())
        }
    }
    struct ReleaseSource(Arc<SourceGate>);
    impl Drop for ReleaseSource {
        fn drop(&mut self) { self.0.release(); }
    }

    // Real Node archival and the actual production Plugin/ContextPreparation
    // owner, with an independent source gate to expose the retirement race.
    // No catalogue read invokes observation on behalf of this consumer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recurring_owner_joins_inflight_archive_and_withdraws_exact_registrations() -> Result<()> {
        let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT")
            .map(std::path::PathBuf::from).context("native owner requires persistent evidence root")?;
        let root = base.join(EntityId::new().to_string());
        std::fs::create_dir_all(&root)?;
        let sandbox = shenko_exec_host::PreparedSandbox::fresh("perception-owner", &root)?;
        let store = sandbox.store().clone();
        let node = Arc::new(Mutex::new(crate::node_sync::open_node(sandbox.installation(), &store)?));
        let catalog = Arc::new(CognitionCatalog::new(node.clone()));
        let package = shenko_awareness_core::seed_packages()?.into_iter()
            .find(|package| package.unit_id == INSTALLATION_UNITS).context("resource seed")?;
        catalog.install(&package, &root.join("packages"))?;
        let binding = PerceptionBinding::of(&node.lock().installed_unit_fact(INSTALLATION_UNITS)?
            .context("installed resource seed")?);
        ensure!(matches!(catalog.perception().cached(&binding), PerceptionStatus::Unknown { .. }),
            "unprepared exact revision must be unknown");
        let gate = Arc::new(SourceGate { armed: AtomicBool::new(false), calls: AtomicUsize::new(0),
            entered: Notify::new(), released: std::sync::Mutex::new(false), changed: Condvar::new() });
        let _release = ReleaseSource(gate.clone());
        let reader = gate.clone();
        struct GatedReading { gate: Arc<SourceGate>, binding: PerceptionBinding }
        impl PerceptionObserver for GatedReading {
            fn observe(&self) -> Result<PerceptionFact> {
                Ok(PerceptionFact {
                    binding: self.binding.clone(), source: "runtime:owner-native-proof".into(),
                    target: "installation.local.owner-proof".into(),
                    observed_at: chrono::Utc::now().to_rfc3339(), valid_until: None,
                    summary: self.gate.read()?, evidence_digest: None,
                    perspective: Some(PerceptionPerspective::Own),
                })
            }
        }
        let proof_id = catalog.perception().bind_source(binding.clone(),
            PerceptionSourceId::named("runtime:owner-native-proof")?,
            Arc::new(ArchivedPerceptionSource::new(node.clone(), binding.clone(),
                PerceptionPerspective::Own,
                Arc::new(GatedReading { gate: reader, binding: binding.clone() }))))?;
        let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration("perception-owner-consumer".into()));
        supervisor.publish_services().await?;
        let plugin = supervisor.spawn(RuntimePerceptionPlugin, catalog.clone()).await?;
        let observed: Result<()> = async {
            ensure!(matches!(catalog.perception().cached(&binding), PerceptionStatus::Observed { .. }),
                "startup's actual owned batch must publish its archived resource reading");
            let first = node.lock().installed_unit_fact(INSTALLATION_UNITS)?.context("resource fact")?
                .descriptor.config;
            gate.armed.store(true, Ordering::SeqCst);
            tokio::time::timeout(Duration::from_secs(5), gate.entered.notified()).await
                .context("recurring worker did not revisit the source")?;
            let retiring = supervisor.uninstall(&plugin);
            tokio::pin!(retiring);
            ensure!(tokio::time::timeout(Duration::from_millis(100), &mut retiring).await.is_err(),
                "retirement completed while original archival callback was still running");
            gate.release();
            tokio::time::timeout(Duration::from_secs(5), &mut retiring).await
                .context("original worker retirement did not join")??;
            let calls = gate.calls.load(Ordering::SeqCst);
            ensure!(calls >= 2, "recurring preparation never ran");
            ensure!(catalog.perception().sources(&binding) == vec![PerceptionSourceId::named("runtime:owner-native-proof")?],
                "runtime withdrawal removed a different owner's source or retained its own registration");
            ensure!(node.lock().installed_unit_fact(INSTALLATION_UNITS)?.context("retained resource")?
                .descriptor.config == first, "unchanged content minted a new archived observation");
            ensure!(catalog.perception().unbind(proof_id), "exact proof registration retirement");
            ensure!(matches!(catalog.perception().cached(&binding), PerceptionStatus::Unknown { .. }),
                "withdrawn registrations must not remain selectable");
            tokio::time::sleep(OBSERVATION_REFRESH * 2).await;
            ensure!(gate.calls.load(Ordering::SeqCst) == calls,
                "a source ran after the original generation's joined retirement");
            ensure!(node.lock().installed_unit_fact(INSTALLATION_UNITS)?.context("retired resource")?
                .descriptor.config == first, "durable observation changed after joined retirement");
            Ok(())
        }.await;
        gate.release();
        let retirement = supervisor.stop().await;
        match (observed, retirement) {
            (Ok(()), result) | (result, Ok(())) => result,
            (Err(primary), Err(cleanup)) => Err(primary.context(format!("owner cleanup also failed: {cleanup:#}"))),
        }
    }
}
