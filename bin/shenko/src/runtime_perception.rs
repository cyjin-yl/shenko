//! Application-level consumer using the production Node opener.

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};
    use anyhow::Result;
    use shenko_awareness_core::{CognitionCatalog, INSTALLATION_UNITS, PerceptionBinding, PerceptionSourceId};
    use shenko_exec_host::RuntimePerceptionPlugin;
    use shenko_types::ids::EntityId;
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
            tokio::time::sleep(Duration::from_millis(500)).await;
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
