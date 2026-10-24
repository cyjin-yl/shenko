//! Runtime integration evidence, not full SPEC hook/activation acceptance.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use cordis_core::{Context, FiberState, Service};
use parking_lot::Mutex;
use shenko_kernel::{Plugin, PluginContext, PluginId, PluginMeta, Supervisor};
use shenko_types::ids::RuntimeGeneration;
use tokio::sync::Notify;

fn meta(name: &str) -> PluginMeta {
    PluginMeta {
        id: PluginId::new(),
        name: name.to_owned(),
        version: "1".to_owned(),
        capabilities_provided: vec![],
        capabilities_required: vec![],
    }
}

struct LiveResource(usize);
impl Service for LiveResource {
    const NAME: &str = "test/live-resource";
}

// Unmodified meta/start/stop shape exercises the default compatibility path.
struct LegacyPlugin {
    meta: PluginMeta,
    starts: AtomicUsize,
    stops: AtomicUsize,
}
impl LegacyPlugin {
    fn new() -> Self {
        Self {
            meta: meta("legacy"),
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        }
    }
}
#[async_trait]
impl Plugin for LegacyPlugin {
    fn meta(&self) -> &PluginMeta {
        &self.meta
    }
    async fn start(&self) -> Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct RuntimePlugin {
    meta: PluginMeta,
    starts: AtomicUsize,
    stops: AtomicUsize,
    contexts: Mutex<Vec<Context>>,
    cleaned: Mutex<Vec<PluginContext>>,
    events: Arc<Mutex<Vec<&'static str>>>,
    gate_start: AtomicBool,
    entered: Notify,
    release: Notify,
    fail_start: bool,
    fail_stop: bool,
}
impl RuntimePlugin {
    fn new(gate_start: bool, fail_start: bool, fail_stop: bool) -> Self {
        Self {
            meta: meta("runtime-plugin"),
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            contexts: Mutex::new(vec![]),
            cleaned: Mutex::new(vec![]),
            events: Arc::new(Mutex::new(vec![])),
            gate_start: AtomicBool::new(gate_start),
            entered: Notify::new(),
            release: Notify::new(),
            fail_start,
            fail_stop,
        }
    }
    fn root(&self) -> Context {
        self.contexts.lock().last().unwrap().root()
    }
}
#[async_trait]
impl Plugin for RuntimePlugin {
    fn meta(&self) -> &PluginMeta {
        &self.meta
    }
    async fn start(&self) -> Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.events.lock().push("start");
        Ok(())
    }
    async fn start_with_context(&self, ctx: Context) -> Result<()> {
        self.start().await?;
        self.contexts.lock().push(ctx.clone());
        // Both are actual cordis registrations. Dropping their returned
        // capabilities leaves cleanup armed in the owning generation.
        let _publication =
            ctx.provide(Arc::new(LiveResource(self.starts.load(Ordering::SeqCst))))?;
        let events = self.events.clone();
        ctx.effect_sync(move || {
            events.lock().push("cordis-effect");
        })?;
        if self.gate_start.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        anyhow::ensure!(!self.fail_start, "partial startup failed");
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.events.lock().push("stop");
        anyhow::ensure!(!self.fail_stop, "stop outcome unknown");
        Ok(())
    }
    async fn effect_cleanup(&self, ctx: &PluginContext) -> Result<()> {
        self.cleaned.lock().push(ctx.clone());
        self.events.lock().push("legacy-cleanup");
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_owns_real_fibers_across_abandoned_start_restart_and_stop() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let supervisor = Arc::new(Supervisor::new(RuntimeGeneration::new("test")));
        let legacy = Arc::new(LegacyPlugin::new());
        let plugin = Arc::new(RuntimePlugin::new(true, false, false));
        supervisor.plugins.register(legacy.clone());
        supervisor.plugins.register(plugin.clone());
        let starting = {
            let supervisor = supervisor.clone();
            tokio::spawn(async move { supervisor.start().await })
        };
        plugin.entered.notified().await;
        // Cancellation abandons only the wait, not the registered resource.
        starting.abort();
        assert!(starting.await.unwrap_err().is_cancelled());
        plugin.release.notify_one();
        supervisor.start().await.unwrap();
        assert_eq!(plugin.starts.load(Ordering::SeqCst), 1);
        assert_eq!(legacy.starts.load(Ordering::SeqCst), 1);
        let root = plugin.root();
        assert_eq!(root.try_service::<LiveResource>().unwrap().0, 1);
        let before = root.runtime_snapshot();
        assert_eq!(before.fibers().len(), 3); // root plus the two real plugins
        let fiber = before
            .fibers()
            .iter()
            .find(|f| f.name() == "runtime-plugin")
            .unwrap();
        assert_eq!(fiber.state(), FiberState::Active);
        let fiber_id = fiber.id().clone();
        assert_eq!(before.services().len(), 1);
        assert_eq!(before.services()[0].provider(), &fiber_id);

        supervisor.restart().await.unwrap();
        assert_eq!(plugin.starts.load(Ordering::SeqCst), 2);
        assert_eq!(legacy.starts.load(Ordering::SeqCst), 2);
        assert_eq!(root.try_service::<LiveResource>().unwrap().0, 2);
        let after = root.runtime_snapshot();
        let restarted = after
            .fibers()
            .iter()
            .find(|f| f.name() == "runtime-plugin")
            .unwrap();
        assert_eq!(restarted.id(), &fiber_id); // actual Cordis same-Fiber restart
        assert_ne!(before.services()[0].id(), after.services()[0].id());

        supervisor.stop().await.unwrap();
        supervisor.stop().await.unwrap(); // no duplicate cleanup
        assert!(supervisor.is_shutdown());
        assert!(supervisor.start().await.is_err());
        assert!(root.try_service::<LiveResource>().is_err());
        assert_eq!(root.runtime_snapshot().fibers().len(), 1);
        assert!(root.runtime_snapshot().services().is_empty());
        assert_eq!(plugin.stops.load(Ordering::SeqCst), 2);
        assert_eq!(legacy.stops.load(Ordering::SeqCst), 2);
        let cleaned = plugin.cleaned.lock();
        assert_eq!(
            cleaned.iter().map(|ctx| ctx.generation).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(cleaned[0].fiber_id, cleaned[1].fiber_id);
        assert_eq!(
            *plugin.events.lock(),
            vec![
                "start",
                "cordis-effect",
                "stop",
                "legacy-cleanup",
                "start",
                "cordis-effect",
                "stop",
                "legacy-cleanup",
            ]
        );
    })
    .await
    .expect("lifecycle must complete without graph-lock deadlock");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_addition_cleans_real_resources_preserves_old_fiber_and_fences_unknown_stop() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let supervisor = Supervisor::new(RuntimeGeneration::new("test"));
        let legacy = Arc::new(LegacyPlugin::new());
        supervisor.plugins.register(legacy.clone());
        supervisor.start().await.unwrap();
        let plugin = Arc::new(RuntimePlugin::new(false, true, true));
        supervisor.plugins.register(plugin.clone());
        let failure = supervisor.start().await.unwrap_err();
        assert!(format!("{failure:#}").contains("partial startup failed"));
        assert!(format!("{failure:#}").contains("stop outcome unknown"));
        let root = plugin.root();
        // The failed Fiber is unlinked by cordis; native publication/effect
        // cleanup and the legacy fallback still run despite stop's error.
        assert!(root.try_service::<LiveResource>().is_err());
        assert!(root.runtime_snapshot().services().is_empty());
        let snapshot = root.runtime_snapshot();
        assert_eq!(snapshot.fibers().len(), 2);
        assert!(
            snapshot
                .fibers()
                .iter()
                .any(|f| f.name() == "legacy" && f.state() == FiberState::Active)
        );
        assert_eq!(legacy.starts.load(Ordering::SeqCst), 1);
        assert_eq!(legacy.stops.load(Ordering::SeqCst), 0);
        assert_eq!(plugin.cleaned.lock().len(), 1);
        assert_eq!(
            *plugin.events.lock(),
            vec!["start", "cordis-effect", "stop", "legacy-cleanup"]
        );
        // Reading the error does not reconcile an external outcome. Neither
        // start nor restart may silently retry the partial legacy startup.
        assert!(supervisor.start().await.is_err());
        assert!(supervisor.restart().await.is_err());
        assert_eq!(plugin.starts.load(Ordering::SeqCst), 1);
        assert_eq!(legacy.stops.load(Ordering::SeqCst), 0);
        assert!(supervisor.stop().await.is_err()); // attempts remaining teardown
        assert_eq!(legacy.stops.load(Ordering::SeqCst), 1);
        assert_eq!(plugin.stops.load(Ordering::SeqCst), 1);
        assert_eq!(root.runtime_snapshot().fibers().len(), 1);
    })
    .await
    .expect("failed startup must reach a determinate cleanup barrier");
}
