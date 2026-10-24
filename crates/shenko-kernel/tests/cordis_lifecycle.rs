//! Actual pinned Cordis runtime evidence, not full hook/activation acceptance.
use std::borrow::Cow;
use std::convert::Infallible;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use cordis_core::{Context, FiberState, InjectSpec, Plugin, Service};
use futures::FutureExt;
use shenko_kernel::Supervisor;
use shenko_types::ids::RuntimeGeneration;
use tokio::sync::{Notify, oneshot};

type Error = std::io::Error;

struct LiveResource(usize);
impl Service for LiveResource {
    const NAME: &str = "test/live-resource";
}

#[derive(Default)]
struct Consumer {
    applies: AtomicUsize,
    cleanups: Arc<AtomicUsize>,
}
impl Plugin for Consumer {
    type Config = ();
    type Input = ();
    type PrepareError = Infallible;
    type ApplyError = Error;
    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(LiveResource::NAME)
    }
    fn prepare(&self, (): ()) -> Result<(), Infallible> {
        Ok(())
    }
    async fn apply(&self, ctx: Context, (): &()) -> Result<(), Error> {
        assert!(ctx.try_service::<LiveResource>().map_err(Error::other)?.0 > 0);
        self.applies.fetch_add(1, Ordering::SeqCst);
        let cleanups = self.cleanups.clone();
        ctx.effect_sync(move || {
            cleanups.fetch_add(1, Ordering::SeqCst);
        })
        .map_err(Error::other)?;
        Ok(())
    }
}

struct RuntimePlugin {
    applies: AtomicUsize,
    joined: Arc<AtomicUsize>,
    cleanups: Arc<AtomicUsize>,
    reentries: Arc<AtomicUsize>,
    host: Weak<Supervisor>,
    gate: AtomicBool,
    entered: Notify,
    release: Notify,
    fail: bool,
}
impl RuntimePlugin {
    fn new(host: &Arc<Supervisor>, gate: bool, fail: bool) -> Self {
        Self {
            applies: AtomicUsize::new(0),
            joined: Arc::new(AtomicUsize::new(0)),
            cleanups: Arc::new(AtomicUsize::new(0)),
            reentries: Arc::new(AtomicUsize::new(0)),
            host: Arc::downgrade(host),
            gate: AtomicBool::new(gate),
            entered: Notify::new(),
            release: Notify::new(),
            fail,
        }
    }
}
impl Plugin for RuntimePlugin {
    type Config = ();
    type Input = ();
    type PrepareError = Infallible;
    type ApplyError = Error;
    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("runtime-plugin")
    }
    fn prepare(&self, (): ()) -> Result<(), Infallible> {
        Ok(())
    }
    async fn apply(&self, ctx: Context, (): &()) -> Result<(), Error> {
        let occurrence = self.applies.fetch_add(1, Ordering::SeqCst) + 1;
        let cleanups = self.cleanups.clone();
        ctx.effect_sync(move || {
            cleanups.fetch_add(1, Ordering::SeqCst);
        })
        .map_err(Error::other)?;
        let _publication = ctx
            .provide(Arc::new(LiveResource(occurrence)))
            .map_err(Error::other)?;
        let (signal, stopped) = oneshot::channel();
        let joined = self.joined.clone();
        ctx.run(async move {
            let _ = stopped.await;
            joined.fetch_add(1, Ordering::SeqCst);
        })
        .map_err(Error::other)?;
        // Native LIFO composition: teardown signal AFTER run's join registration.
        // There is no legacy stop callback registered before the joined task.
        ctx.effect_sync(move || {
            let _ = signal.send(());
        })
        .map_err(Error::other)?;
        let host = self.host.clone();
        let reentries = self.reentries.clone();
        ctx.effect(move || async move {
            if let Some(host) = host.upgrade() {
                if host.is_shutdown() {
                    assert!(host.stop().await.is_err());
                    reentries.fetch_add(1, Ordering::SeqCst);
                }
            }
        })
        .map_err(Error::other)?;
        if self.fail {
            // Display panics with a payload whose Drop also panics. Real Cordis
            // contains both, still attempts other cleanup, and emits an owned log.
            ctx.effect_sync(|| Err::<(), PoisonedDisplay>(PoisonedDisplay))
                .map_err(Error::other)?;
        }
        if self.gate.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if self.fail {
            return Err(std::io::Error::other("partial apply failed").into());
        }
        Ok(())
    }
}

#[derive(Debug)]
struct PoisonedDisplay;
impl fmt::Display for PoisonedDisplay {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        std::panic::panic_any(PoisonedPayload)
    }
}
impl std::error::Error for PoisonedDisplay {}
struct PoisonedPayload;
impl Drop for PoisonedPayload {
    fn drop(&mut self) {
        panic!("payload destructor failed")
    }
}

struct Healthy(Arc<AtomicUsize>);
impl Plugin for Healthy {
    type Config = ();
    type Input = ();
    type PrepareError = Infallible;
    type ApplyError = Error;
    fn prepare(&self, (): ()) -> Result<(), Infallible> {
        Ok(())
    }
    async fn apply(&self, ctx: Context, (): &()) -> Result<(), Error> {
        let cleaned = self.0.clone();
        ctx.effect_sync(move || {
            cleaned.fetch_add(1, Ordering::SeqCst);
        })
        .map_err(Error::other)?;
        Ok(())
    }
}

struct BadDeclaration;
impl Plugin for BadDeclaration {
    type Config = ();
    type Input = ();
    type PrepareError = Infallible;
    type ApplyError = Infallible;
    fn name(&self) -> Cow<'_, str> {
        panic!("native name panic before admission")
    }
    fn prepare(&self, (): ()) -> Result<(), Infallible> {
        Ok(())
    }
    async fn apply(&self, _: Context, (): &()) -> Result<(), Infallible> {
        unreachable!()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_services_restart_join_order_reentry_and_terminal_shutdown() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let host = Arc::new(Supervisor::new(RuntimeGeneration::new("test")));
        let consumer = Arc::new(Consumer::default());
        let dependent = host.spawn(consumer.clone(), ()).await.unwrap();
        assert_eq!(dependent.state(), FiberState::Pending);
        let plugin = Arc::new(RuntimePlugin::new(&host, false, false));
        let provider = host.spawn(plugin.clone(), ()).await.unwrap();
        dependent
            .wait_state(FiberState::Active, Duration::from_secs(2))
            .await
            .unwrap();
        let root = host.context();
        let before = root.runtime_snapshot();
        assert_eq!(root.try_service::<LiveResource>().unwrap().0, 1);
        let provider_id = provider.id();
        assert_eq!(before.services()[0].provider(), &provider_id);
        host.restart(&provider).await.unwrap();
        dependent
            .wait_state(FiberState::Active, Duration::from_secs(2))
            .await
            .unwrap();
        let after = root.runtime_snapshot();
        assert_eq!(root.try_service::<LiveResource>().unwrap().0, 2);
        assert_eq!(provider.id(), provider_id);
        assert_ne!(before.services()[0].id(), after.services()[0].id());
        assert_eq!(plugin.joined.load(Ordering::SeqCst), 1);
        let cleaned = Arc::new(AtomicUsize::new(0));
        host.stop().await.unwrap();
        host.stop().await.unwrap();
        assert!(host.is_shutdown());
        assert!(host.spawn(Healthy(cleaned.clone()), ()).await.is_err());
        assert!(root.try_service::<LiveResource>().is_err());
        assert_eq!(root.runtime_snapshot().fibers().len(), 1);
        assert!(root.runtime_snapshot().services().is_empty());
        assert_eq!(plugin.applies.load(Ordering::SeqCst), 2);
        assert_eq!(plugin.joined.load(Ordering::SeqCst), 2);
        assert_eq!(plugin.cleanups.load(Ordering::SeqCst), 2);
        assert_eq!(plugin.reentries.load(Ordering::SeqCst), 1);
        assert_eq!(cleaned.load(Ordering::SeqCst), 0);
        assert_eq!(
            consumer.cleanups.load(Ordering::SeqCst),
            consumer.applies.load(Ordering::SeqCst)
        );
    })
    .await
    .expect("native lifecycle, cleanup reentry and task joins must complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_failure_isolation_panic_containment_and_unknown_cleanup_latch() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let host = Arc::new(Supervisor::new(RuntimeGeneration::new("test")));
        let cleaned = Arc::new(AtomicUsize::new(0));
        let healthy = host.spawn(Healthy(cleaned.clone()), ()).await.unwrap();
        assert!(
            AssertUnwindSafe(host.spawn(BadDeclaration, ()))
                .catch_unwind()
                .await
                .is_err()
        );
        assert_eq!(healthy.state(), FiberState::Active);
        let plugin = Arc::new(RuntimePlugin::new(&host, false, true));
        let failure = host.spawn(plugin.clone(), ()).await.unwrap_err();
        assert!(format!("{failure:#}").contains("partial apply failed"));
        assert_eq!(healthy.state(), FiberState::Active);
        assert_eq!(cleaned.load(Ordering::SeqCst), 0);
        assert_eq!(plugin.cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(plugin.joined.load(Ordering::SeqCst), 1);
        assert!(host.context().runtime_snapshot().services().is_empty());
        assert!(
            host.plugins
                .diagnostics()
                .iter()
                .any(|r| r.text().contains("payload destructor panicked"))
        );
        assert!(host.spawn(Healthy(cleaned.clone()), ()).await.is_err());
        assert!(host.restart(&healthy).await.is_err());
        assert!(host.stop().await.is_err()); // drain != proof external effects undone
        assert!(host.stop().await.is_err()); // reading does not clear unknown
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert_eq!(host.context().runtime_snapshot().fibers().len(), 1);
    })
    .await
    .expect("native failure must not lose an owner or skip remaining cleanup");
}

// One added regression: stop must interrupt an unfinished native apply and
// wait its real cleanup, even after the original creation waiter went away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_interrupts_abandoned_pending_creation_and_closes_admission() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let host = Arc::new(Supervisor::new(RuntimeGeneration::new("test")));
        let cleaned = Arc::new(AtomicUsize::new(0));
        host.spawn(Healthy(cleaned.clone()), ()).await.unwrap();
        let plugin = Arc::new(RuntimePlugin::new(&host, true, false));
        let starting = {
            let host = host.clone();
            let plugin = plugin.clone();
            tokio::spawn(async move { host.spawn(plugin, ()).await })
        };
        plugin.entered.notified().await;
        starting.abort();
        assert!(starting.await.unwrap_err().is_cancelled());
        // No release signal: shutdown must drop the pending native spawn,
        // then typed removal must await Cordis's rollback/unlink barrier.
        let (stopped, late_start) =
            tokio::join!(host.stop(), host.spawn(Healthy(cleaned.clone()), ()));
        stopped.unwrap();
        assert!(late_start.is_err());
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert_eq!(plugin.applies.load(Ordering::SeqCst), 1);
        assert_eq!(plugin.joined.load(Ordering::SeqCst), 1);
        assert_eq!(plugin.cleanups.load(Ordering::SeqCst), 1);
        assert!(host.context().runtime_snapshot().services().is_empty());
        assert_eq!(host.context().runtime_snapshot().fibers().len(), 1);
    })
    .await
    .expect("pending creation cannot trap shutdown behind apply");
}
