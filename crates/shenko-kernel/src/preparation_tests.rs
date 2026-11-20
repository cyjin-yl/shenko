use super::*;
use cordis_core::{InjectSpec, Service};
use std::{fs::File, io::Write as _, path::PathBuf, sync::{Condvar, atomic::AtomicBool}, time::Duration};
use tokio::sync::oneshot;

struct NativeGate {
    entered: Notify,
    thread: Mutex<Option<std::thread::ThreadId>>,
    open: std::sync::Mutex<bool>,
    changed: Condvar,
}
impl NativeGate {
    fn new(open: bool) -> Self { Self { entered: Notify::new(), thread: Mutex::new(None), open: std::sync::Mutex::new(open), changed: Condvar::new() } }
    fn wait(&self) {
        *self.thread.lock() = Some(std::thread::current().id());
        self.entered.notify_one();
        let open = self.open.lock().expect("native gate");
        let (open, _) = self.changed.wait_timeout_while(open, Duration::from_secs(15), |open| !*open).expect("native gate wait");
        assert!(*open, "native ownership consumer gate was not released");
    }
    fn release(&self) { *self.open.lock().expect("native gate") = true; self.changed.notify_all(); }
}
struct DropControl { gate: NativeGate, count: AtomicUsize, panic_after_close: AtomicBool }
impl DropControl {
    fn new(open: bool) -> Arc<Self> {
        Arc::new(Self { gate: NativeGate::new(open), count: AtomicUsize::new(0), panic_after_close: AtomicBool::new(false) })
    }
}
struct Dependency;
impl Service for Dependency { const NAME: &'static str = "kernel.preparation.test.dependency"; }
struct DependencyPlugin;
impl Plugin for DependencyPlugin {
    type Config = Arc<AtomicUsize>; type Input = Self::Config;
    type PrepareError = std::convert::Infallible; type ApplyError = std::io::Error;
    fn name(&self) -> std::borrow::Cow<'_, str> { "preparation-real-dependency".into() }
    fn prepare(&self, input: Self::Config) -> std::result::Result<Self::Input, Self::PrepareError> { Ok(input) }
    async fn apply(&self, ctx: Context, withdrawals: &Self::Input) -> std::io::Result<()> {
        ctx.provide(Arc::new(Dependency)).map_err(std::io::Error::other)?;
        let withdrawals = withdrawals.clone();
        ctx.effect(move || async move { withdrawals.fetch_add(1, Ordering::SeqCst); }).map_err(std::io::Error::other)?;
        Ok(())
    }
}
struct ReentrantStopPlugin;
impl Plugin for ReentrantStopPlugin {
    type Config = (std::sync::Weak<crate::PluginRegistry>, Arc<AtomicUsize>);
    type Input = Self::Config;
    type PrepareError = std::convert::Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> std::borrow::Cow<'_, str> { "preparation-stop-reentrant-control".into() }
    fn prepare(&self, input: Self::Config) -> std::result::Result<Self::Input, Self::PrepareError> { Ok(input) }
    async fn apply(&self, ctx: Context, input: &Self::Input) -> std::io::Result<()> {
        let (registry, refusals) = input.clone();
        ctx.effect(move || async move {
            let registry = registry.upgrade().expect("original stop registry remains owned");
            assert!(registry.stop().await.is_err(), "active cleanup must not obtain its own stop observation");
            refusals.fetch_add(1, Ordering::SeqCst);
        }).map_err(std::io::Error::other)?;
        Ok(())
    }
}
struct InitialRecursiveStopPlugin;
impl Plugin for InitialRecursiveStopPlugin {
    type Config = (std::sync::Weak<crate::PluginRegistry>, Arc<Mutex<Option<crate::StopObservation>>>, Arc<AtomicUsize>);
    type Input = Self::Config;
    type PrepareError = std::convert::Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> std::borrow::Cow<'_, str> { "preparation-initial-stop-recursion".into() }
    fn prepare(&self, input: Self::Config) -> std::result::Result<Self::Input, Self::PrepareError> { Ok(input) }
    async fn apply(&self, ctx: Context, input: &Self::Input) -> std::io::Result<()> {
        let registry = input.0.upgrade().expect("native caller owns registry");
        let observation = registry.begin_stop().map_err(std::io::Error::other)?;
        let failure = registry.wait_stopped(&observation).await.expect_err("actual native self-wait must refuse before terminal commit");
        assert!(failure.chain().any(|cause| cause.downcast_ref::<cordis_core::lifecycle::LifecycleRecursion>().is_some()));
        *input.1.lock() = Some(observation);
        let retired = input.2.clone();
        ctx.effect(move || async move { retired.fetch_add(1, Ordering::SeqCst); }).map_err(std::io::Error::other)?;
        Ok(())
    }
}
struct FileProduct { file: Option<File>, control: Arc<DropControl>, _dependency: Option<Arc<Dependency>> }
impl Drop for FileProduct {
    fn drop(&mut self) {
        self.control.gate.wait();
        drop(self.file.take());
        self.control.count.fetch_add(1, Ordering::SeqCst);
        if self.control.panic_after_close.load(Ordering::SeqCst) { std::panic::panic_any("actual prepared FileProduct destructor panic"); }
    }
}
struct ResourceFailure {
    marker: EntityId,
    _resource: FileProduct,
    source: std::io::Error,
}
impl std::fmt::Debug for ResourceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceFailure").field("marker", &self.marker).finish_non_exhaustive()
    }
}
impl std::fmt::Display for ResourceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "original resource-bearing failure {}", self.marker) }
}
impl std::error::Error for ResourceFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(&self.source) }
}
struct ProductWithResourcePanic { product: Option<FileProduct>, payload: Option<ResourceFailure> }
impl Drop for ProductWithResourcePanic {
    fn drop(&mut self) {
        drop(self.product.take());
        std::panic::panic_any(self.payload.take().expect("original owned destructor panic payload"));
    }
}
fn root(label: &str) -> Result<PathBuf> {
    let root = std::env::var_os("SHENKO_PREPARATION_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/owned-preparation"))
        .join(format!("{label}-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    Ok(root)
}
fn product(path: PathBuf, control: Arc<DropControl>, dependency: Option<Arc<Dependency>>) -> Result<FileProduct> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(b"actual owned preparation product")?;
    Ok(FileProduct { file: Some(file), control, _dependency: dependency })
}
async fn finished_without_delivery<T: Send + Sync + 'static>(owner: &ContextPreparation<T>, cutoff: tokio::time::Instant) -> Result<()> {
    tokio::time::timeout_at(cutoff, async {
        loop {
            let finished = { let slot = owner.state.work.lock().await; matches!(&*slot, Work::Running(task) if task.is_finished()) };
            if finished { break; }
            tokio::task::yield_now().await;
        }
    }).await?;
    Ok(())
}

struct PreparingConfig {
    path: PathBuf, control: Arc<DropControl>,
    owner: Arc<Mutex<Option<oneshot::Sender<Arc<ContextPreparation<FileProduct>>>>>>,
}
struct PreparingPlugin;
impl Plugin for PreparingPlugin {
    type Config = PreparingConfig; type Input = Self::Config;
    type PrepareError = std::convert::Infallible; type ApplyError = std::io::Error;
    fn name(&self) -> std::borrow::Cow<'_, str> { "preparation-actual-dependent-scope".into() }
    fn inject(&self) -> InjectSpec { InjectSpec::none().require(Dependency::NAME) }
    fn prepare(&self, config: Self::Config) -> std::result::Result<Self::Input, Self::PrepareError> { Ok(config) }
    async fn apply(&self, ctx: Context, input: &Self::Input) -> std::io::Result<()> {
        let owner = ContextPreparation::<FileProduct>::attach(&ctx).await.map_err(std::io::Error::other)?;
        let permit = owner.reserve_start().map_err(std::io::Error::other)?;
        let dependency = ctx.try_service::<Dependency>().map_err(std::io::Error::other)?;
        let path = input.path.clone(); let control = input.control.clone();
        permit.start(move |cancel| { cancel.checkpoint()?; product(path, control, Some(dependency)) }).map_err(std::io::Error::other)?;
        input.owner.lock().take().ok_or_else(|| std::io::Error::other("original owner receiver already consumed"))?
            .send(owner).map_err(|_| std::io::Error::other("actual owner observer disappeared"))?;
        Ok(())
    }
}

#[tokio::test]
async fn completed_undelivered_product_keeps_native_disposal_and_dependency_barrier_after_stop_observer_cancel() -> Result<()> {
    let dir = root("completed-undelivered")?;
    let cutoff = tokio::time::Instant::now() + Duration::from_secs(15);
    let supervisor = Arc::new(crate::Supervisor::new(shenko_types::ids::RuntimeGeneration("preparation-scope".into())));
    let withdrawals = Arc::new(AtomicUsize::new(0));
    supervisor.spawn(DependencyPlugin, withdrawals.clone()).await?;
    let control = DropControl::new(false);
    let (owner_tx, owner_rx) = oneshot::channel();
    supervisor.spawn(PreparingPlugin, PreparingConfig { path: dir.join("product"), control: control.clone(),
        owner: Arc::new(Mutex::new(Some(owner_tx))) }).await?;
    let owner = tokio::time::timeout_at(cutoff, owner_rx).await??;
    finished_without_delivery(&owner, cutoff).await?;
    let startup = ContextPreparation::<(FileProduct, u8)>::attach_to(&supervisor).await?;
    let startup_control = DropControl::new(false);
    let startup_drop = startup_control.clone();
    let startup_dependency = supervisor.context().try_service::<Dependency>()?;
    startup.reserve_start()?.start(move |_| Ok((product(dir.join("startup-product"), startup_drop, Some(startup_dependency))?, 7)))?;
    finished_without_delivery(&startup, cutoff).await?;
    assert_eq!(supervisor.context().runtime_snapshot().fibers().iter()
        .filter(|fiber| fiber.name() == "kernel-owned-preparation").count(), 2,
        "generation and startup owners must be actual native resident scopes");
    supervisor.hooks.await_current_quiescence().await;
    let original_stop = supervisor.plugins.begin_stop()?;
    let first_ticket = original_stop.clone();
    let stopping_supervisor = supervisor.clone();
    let stopping = tokio::spawn(async move { stopping_supervisor.plugins.wait_stopped(&first_ticket).await });
    tokio::time::timeout_at(cutoff, control.gate.entered.notified()).await?;
    tokio::time::timeout_at(cutoff, tokio::time::sleep(Duration::from_millis(20))).await?;
    assert_ne!(*control.gate.thread.lock(), Some(std::thread::current().id()));
    assert_eq!(withdrawals.load(Ordering::SeqCst), 0, "declared dependency withdrew through a live primitive disposal");
    assert!(supervisor.context().try_service::<Dependency>().is_ok(), "actual provider binding must stay published during disposal");
    assert!(supervisor.plugins.begin_stop().is_err(), "an active stop must not mint another cleanup observation ticket");
    assert!(supervisor.stop().await.is_err(), "ordinary concurrent/reentrant stop must retain its refusal");
    stopping.abort(); assert!(stopping.await.expect_err("outer stop observer cancelled").is_cancelled());
    assert_eq!(control.count.load(Ordering::SeqCst), 0);
    let reobserving_supervisor = supervisor.clone();
    let reobserving = tokio::spawn(async move { reobserving_supervisor.plugins.wait_stopped(&original_stop).await });
    tokio::task::yield_now().await;
    assert!(supervisor.context().try_service::<Dependency>().is_ok(), "observer cancellation must not withdraw the actual provider binding");
    control.gate.release();
    tokio::time::timeout_at(cutoff, owner.close()).await??;
    tokio::time::timeout_at(cutoff, startup_control.gate.entered.notified()).await?;
    assert!(supervisor.context().try_service::<Dependency>().is_ok(), "all product types in the native group must settle before dependency withdrawal");
    assert_eq!(withdrawals.load(Ordering::SeqCst), 0);
    assert_eq!(control.count.load(Ordering::SeqCst), 1);
    assert_ne!(*startup_control.gate.thread.lock(), Some(std::thread::current().id()));
    startup_control.gate.release();
    tokio::time::timeout_at(cutoff, reobserving).await???;
    tokio::time::timeout_at(cutoff, async {
        while withdrawals.load(Ordering::SeqCst) == 0 { tokio::task::yield_now().await; }
    }).await?;
    assert_eq!(control.count.load(Ordering::SeqCst), 1);
    assert_eq!(startup_control.count.load(Ordering::SeqCst), 1);
    assert_eq!(withdrawals.load(Ordering::SeqCst), 1);
    assert!(supervisor.context().try_service::<Dependency>().is_err());
    assert!(matches!(*owner.state.work.lock().await, Work::Retired));
    assert!(!supervisor.context().runtime_snapshot().fibers().iter()
        .any(|fiber| fiber.name() == "kernel-owned-preparation"));
    Ok(())
}

#[tokio::test]
async fn cancelled_attach_observer_retains_original_preadmission_task_until_shutdown_refusal() -> Result<()> {
    let cutoff = tokio::time::Instant::now() + Duration::from_secs(15);
    let supervisor = Arc::new(crate::Supervisor::new(shenko_types::ids::RuntimeGeneration("cancelled-attach".into())));
    let withdrawals = Arc::new(AtomicUsize::new(0));
    supervisor.spawn(DependencyPlugin, withdrawals.clone()).await?;
    let admission = supervisor.context().try_service::<PreparationAdmission>()?;
    let baseline = Arc::strong_count(&admission);
    let held = admission.open.lock().await;
    let attaching_supervisor = supervisor.clone();
    let attaching = tokio::spawn(async move { ContextPreparation::<()>::attach_to(&attaching_supervisor).await });
    tokio::time::timeout_at(cutoff, async {
        while Arc::strong_count(&admission) == baseline { tokio::task::yield_now().await; }
    }).await?;
    attaching.abort();
    assert!(matches!(attaching.await, Err(error) if error.is_cancelled()),
        "original attach observer cancelled before admission");
    assert!(Arc::strong_count(&admission) > baseline,
        "the attributed admission task, not the cancelled observer, must retain the original gate authority");
    assert!(!supervisor.context().runtime_snapshot().fibers().iter()
        .any(|fiber| fiber.name() == "kernel-owned-preparation"), "this is a preadmission suspension, not a manufactured scope-creation await");
    supervisor.hooks.await_current_quiescence().await;
    let ticket = supervisor.plugins.begin_stop()?;
    let observing = supervisor.clone();
    let stopped = tokio::spawn(async move { observing.plugins.wait_stopped(&ticket).await });
    tokio::task::yield_now().await;
    assert!(!stopped.is_finished());
    assert!(supervisor.context().try_service::<Dependency>().is_ok());
    assert_eq!(withdrawals.load(Ordering::SeqCst), 0);
    drop(held);
    tokio::time::timeout_at(cutoff, stopped).await???;
    tokio::time::timeout_at(cutoff, async {
        while Arc::strong_count(&admission) != baseline { tokio::task::yield_now().await; }
    }).await?;
    assert!(admission.shutdown.is_cancelled());
    assert!(!supervisor.context().runtime_snapshot().fibers().iter()
        .any(|fiber| fiber.name() == "kernel-owned-preparation"));
    assert_eq!(withdrawals.load(Ordering::SeqCst), 1);
    assert!(matches!(ContextPreparation::<()>::attach_to(&supervisor).await,
        Err(PreparationError::State { kind: PreparationStateError::Closed, .. })));
    Ok(())
}

#[tokio::test]
async fn admitted_native_start_racing_stop_keeps_same_worker_through_cancelled_product_drop() -> Result<()> {
    let dir = root("admitted-start-stop")?;
    let cutoff = tokio::time::Instant::now() + Duration::from_secs(15);
    let supervisor = Arc::new(crate::Supervisor::new(shenko_types::ids::RuntimeGeneration("admitted-start-stop".into())));
    let withdrawals = Arc::new(AtomicUsize::new(0));
    supervisor.spawn(DependencyPlugin, withdrawals.clone()).await?;
    let owner = ContextPreparation::<FileProduct>::attach_to(&supervisor).await?;
    let original_work = Arc::new(NativeGate::new(false)); let native_work = original_work.clone();
    let control = DropControl::new(false); let native_control = control.clone();
    let dependency = supervisor.context().try_service::<Dependency>()?;
    owner.reserve_start()?.start(move |_| {
        let prepared = product(dir.join("original-admitted-product"), native_control, Some(dependency))?;
        native_work.wait();
        Ok(prepared)
    })?;
    tokio::time::timeout_at(cutoff, original_work.entered.notified()).await?;
    supervisor.hooks.await_current_quiescence().await;
    let ticket = supervisor.plugins.begin_stop()?;
    tokio::time::timeout_at(cutoff, owner.state.cancellation.cancelled()).await?;
    assert!(supervisor.context().try_service::<Dependency>().is_ok());
    original_work.release();
    tokio::time::timeout_at(cutoff, control.gate.entered.notified()).await?;
    assert_eq!(*original_work.thread.lock(), *control.gate.thread.lock(),
        "a cancelled successful callback must destroy its product on its original native worker");
    assert_ne!(*control.gate.thread.lock(), Some(std::thread::current().id()));
    assert_eq!(control.count.load(Ordering::SeqCst), 0);
    assert_eq!(withdrawals.load(Ordering::SeqCst), 0);
    assert!(supervisor.context().try_service::<Dependency>().is_ok());
    let first_observer = supervisor.clone(); let first_ticket = ticket.clone();
    let waiting = tokio::spawn(async move { first_observer.plugins.wait_stopped(&first_ticket).await });
    tokio::task::yield_now().await;
    waiting.abort(); assert!(waiting.await.expect_err("admitted-work stop observer cancelled").is_cancelled());
    control.gate.release();
    tokio::time::timeout_at(cutoff, supervisor.plugins.wait_stopped(&ticket)).await??;
    assert_eq!(control.count.load(Ordering::SeqCst), 1);
    assert_eq!(withdrawals.load(Ordering::SeqCst), 1);
    owner.close().await?;
    Ok(())
}

#[tokio::test]
async fn reserved_startup_scope_blocks_stop_and_refuses_late_preparation_admission() -> Result<()> {
    let cutoff = tokio::time::Instant::now() + Duration::from_secs(15);
    let supervisor = Arc::new(crate::Supervisor::new(shenko_types::ids::RuntimeGeneration("reservation-stop".into())));
    let withdrawals = Arc::new(AtomicUsize::new(0));
    supervisor.spawn(DependencyPlugin, withdrawals.clone()).await?;
    let owner = ContextPreparation::<()>::attach_to(&supervisor).await?;
    let reservation = owner.reserve_start()?;
    let observing = supervisor.clone();
    let stopping = tokio::spawn(async move { observing.stop().await });
    tokio::time::timeout_at(cutoff, owner.state.cancellation.cancelled()).await?;
    assert!(supervisor.context().try_service::<Dependency>().is_ok());
    assert_eq!(withdrawals.load(Ordering::SeqCst), 0);
    let late = tokio::time::timeout_at(cutoff, ContextPreparation::<()>::attach_to(&supervisor)).await?
        .err().ok_or_else(|| anyhow::anyhow!("preparation admitted after original stop closed the gate"))?;
    assert!(matches!(late, PreparationError::State { kind: PreparationStateError::Closed, .. }));
    assert!(matches!(owner.reserve_start(), Err(PreparationError::State { kind: PreparationStateError::Closed, .. })));
    drop(reservation);
    tokio::time::timeout_at(cutoff, stopping).await???;
    assert_eq!(withdrawals.load(Ordering::SeqCst), 1);
    assert!(supervisor.context().try_service::<Dependency>().is_err());
    supervisor.stop().await?;
    Ok(())
}

#[tokio::test]
async fn native_cleanup_cannot_mint_its_own_stop_observation() -> Result<()> {
    let registry = Arc::new(crate::PluginRegistry::new(CancellationToken::new()));
    let refusals = Arc::new(AtomicUsize::new(0));
    registry.spawn(ReentrantStopPlugin, (Arc::downgrade(&registry), refusals.clone())).await?;
    let observation = registry.begin_stop()?;
    tokio::time::timeout(Duration::from_secs(15), registry.wait_stopped(&observation)).await??;
    assert_eq!(refusals.load(Ordering::SeqCst), 1);
    registry.wait_stopped(&observation.clone()).await?;
    Ok(())
}

#[tokio::test]
async fn actual_native_precommit_recursion_keeps_original_ticket_and_allows_external_recovery() -> Result<()> {
    let supervisor = crate::Supervisor::new(shenko_types::ids::RuntimeGeneration("native-recursion-closed-gate".into()));
    let registry = supervisor.plugins.clone();
    let startup = ContextPreparation::<()>::attach_to(&supervisor).await?;
    let original = Arc::new(Mutex::new(None));
    let retired = Arc::new(AtomicUsize::new(0));
    let input = (Arc::downgrade(&registry), original.clone(), retired.clone());
    // Direct native composition keeps this original apply alive long enough to
    // observe Cordis's real refusal; the host's shutdown select would cancel it.
    tokio::time::timeout(Duration::from_secs(15), registry.context().spawn(
        PreparedPlugin::from_input(InitialRecursiveStopPlugin, input))).await??;
    let observation = original.lock().take().ok_or_else(|| anyhow::anyhow!("native apply did not return its original refusal ticket"))?;
    let first = registry.wait_stopped(&observation).await.expect_err("original native refusal must remain observable");
    assert!(matches!(startup.reserve_start(), Err(PreparationError::State { kind: PreparationStateError::Closed, .. })));
    assert!(matches!(ContextPreparation::<()>::attach_to(&supervisor).await,
        Err(PreparationError::State { kind: PreparationStateError::Closed, .. })), "native precommit refusal must never reopen preparation admission");
    tokio::time::timeout(Duration::from_secs(15), registry.stop()).await??;
    assert_eq!(retired.load(Ordering::SeqCst), 1);
    assert!(matches!(ContextPreparation::<()>::attach_to(&supervisor).await,
        Err(PreparationError::State { kind: PreparationStateError::Closed, .. })), "external recovery must keep the original admission closed");
    let repeated = registry.wait_stopped(&observation).await.expect_err("external recovery must not rewrite the original refusal");
    assert!(first.chain().any(|cause| cause.downcast_ref::<cordis_core::lifecycle::LifecycleRecursion>().is_some()));
    assert!(repeated.chain().any(|cause| cause.downcast_ref::<cordis_core::lifecycle::LifecycleRecursion>().is_some()));
    Ok(())
}

#[tokio::test]
async fn resident_startup_scope_and_delivery_lease_keep_final_file_drop_off_reactor() -> Result<()> {
    let dir = root("resident-startup")?;
    let cutoff = tokio::time::Instant::now() + Duration::from_secs(15);
    let supervisor = Arc::new(crate::Supervisor::new(shenko_types::ids::RuntimeGeneration("startup-preparation".into())));
    let owner = ContextPreparation::<FileProduct>::attach_to(&supervisor).await?;
    let control = DropControl::new(false); let native_control = control.clone();
    owner.reserve_start()?.start(move |_| product(dir.join("product"), native_control, None))?;
    let prepared = owner.deliver().await?;
    let observing_owner = owner.clone();
    let closing = tokio::spawn(async move { observing_owner.close().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!closing.is_finished()); assert_eq!(control.count.load(Ordering::SeqCst), 0);
    drop(prepared);
    tokio::time::timeout_at(cutoff, control.gate.entered.notified()).await?;
    tokio::time::timeout_at(cutoff, tokio::time::sleep(Duration::from_millis(20))).await?;
    assert_ne!(*control.gate.thread.lock(), Some(std::thread::current().id()));
    closing.abort(); assert!(closing.await.expect_err("close observer cancelled").is_cancelled());
    assert!(owner.retirement.lock().await.future.is_some());
    control.gate.release();
    tokio::time::timeout_at(cutoff, owner.close()).await??;
    assert_eq!(control.count.load(Ordering::SeqCst), 1);
    supervisor.stop().await?;
    Ok(())
}

#[tokio::test]
async fn closed_start_permit_disposes_captured_authorities_natively_without_running_callback() -> Result<()> {
    let dir = root("closed-reservation")?;
    let cutoff = tokio::time::Instant::now() + Duration::from_secs(15);
    let supervisor = crate::Supervisor::new(shenko_types::ids::RuntimeGeneration("closed-preparation".into()));
    let owner = ContextPreparation::<()>::attach_to(&supervisor).await?;
    let permit = owner.reserve_start()?;
    let control = DropControl::new(false);
    let captured = product(dir.join("captured"), control.clone(), None)?;
    let ran = Arc::new(AtomicBool::new(false)); let running = ran.clone();
    owner.request_close();
    let error = permit.start(move |_| { running.store(true, Ordering::SeqCst); drop(captured); Ok(()) })
        .expect_err("closed original scope must reject execution");
    assert!(matches!(error, PreparationError::State { kind: PreparationStateError::Closed, .. }));
    tokio::time::timeout_at(cutoff, control.gate.entered.notified()).await?;
    tokio::time::timeout_at(cutoff, tokio::time::sleep(Duration::from_millis(20))).await?;
    assert!(!ran.load(Ordering::SeqCst));
    assert_ne!(*control.gate.thread.lock(), Some(std::thread::current().id()));
    control.gate.release();
    tokio::time::timeout_at(cutoff, owner.close()).await??;
    assert_eq!(control.count.load(Ordering::SeqCst), 1);
    supervisor.stop().await?;
    Ok(())
}

#[tokio::test]
async fn cancelled_delivery_and_close_observers_retain_same_native_file_error_and_handle() -> Result<()> {
    let dir = root("original-native-error")?;
    let cutoff = tokio::time::Instant::now() + Duration::from_secs(15);
    let supervisor = Arc::new(crate::Supervisor::new(shenko_types::ids::RuntimeGeneration("error-preparation".into())));
    let owner = ContextPreparation::<()>::attach_to(&supervisor).await?;
    let held = Arc::new(NativeGate::new(false)); let blocking = held.clone();
    owner.reserve_start()?.start(move |_| -> Result<()> {
        let path = dir.join("regular-file"); std::fs::write(&path, b"real native error ancestor")?;
        let _authority = File::open(&path)?;
        blocking.wait();
        File::open(path.join("child"))?;
        Ok(())
    })?;
    tokio::time::timeout_at(cutoff, held.entered.notified()).await?;
    let delivery_owner = owner.clone();
    let delivering = tokio::spawn(async move { delivery_owner.deliver().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    delivering.abort(); assert!(delivering.await.expect_err("actual delivery observer cancelled").is_cancelled());
    let closing_owner = owner.clone();
    let closing = tokio::spawn(async move { closing_owner.close().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    closing.abort(); assert!(closing.await.expect_err("actual close observer cancelled").is_cancelled());
    assert!(owner.retirement.lock().await.future.is_some());
    held.release();
    let error = tokio::time::timeout_at(cutoff, owner.close()).await?.expect_err("actual native primitive error cannot become cancellation/success");
    let PreparationError::Retirement(failure) = &error else { anyhow::bail!("native retirement failure type lost"); };
    let work = failure.work().expect("original work error");
    assert_eq!(work.owner_id(), owner.id()); assert_eq!(work.kind(), PreparationFailureKind::ReturnedError);
    assert!(std::error::Error::source(failure.as_ref()).is_none(), "retired receipt must not claim a destroyed original source");
    assert!(supervisor.stop().await.is_err(), "real native failure must remain visible at kernel stop");
    Ok(())
}

#[tokio::test]
async fn actual_work_panic_and_disposal_panic_remain_typed_original_failures() -> Result<()> {
    let dir = root("native-panics")?;
    let supervisor = crate::Supervisor::new(shenko_types::ids::RuntimeGeneration("panic-preparation".into()));
    let panicking = ContextPreparation::<()>::attach_to(&supervisor).await?;
    panicking.reserve_start()?.start(|_| -> Result<()> { std::panic::panic_any("actual original preparation panic"); })?;
    let error = panicking.deliver().await.expect_err("actual native panic cannot become success");
    let PreparationError::Work(original) = error else { anyhow::bail!("original panic classification lost"); };
    assert_eq!(original.kind(), PreparationFailureKind::Panicked);
    let lease = panicking.observe_failure().await?.ok_or_else(|| anyhow::anyhow!("original panic source missing before retirement"))?;
    assert_eq!(lease.failure_id(), original.failure_id());
    assert!(lease.join_error().is_some_and(JoinError::is_panic));
    drop(lease);
    let closing = panicking.close().await.expect_err("retirement must retain original work panic");
    let PreparationError::Retirement(retired) = closing else { anyhow::bail!("retired panic type lost"); };
    assert_eq!(retired.work().expect("original panic receipt").failure_id(), original.failure_id());
    let disposing = ContextPreparation::<FileProduct>::attach_to(&supervisor).await?;
    let control = DropControl::new(true); control.panic_after_close.store(true, Ordering::SeqCst);
    let dropping = control.clone();
    disposing.reserve_start()?.start(move |_| product(dir.join("panic-product"), dropping, None))?;
    drop(disposing.deliver().await?);
    let disposal = disposing.close().await.expect_err("actual destructor panic cannot become clean retirement");
    let PreparationError::Retirement(first) = disposal else { anyhow::bail!("disposal panic type lost"); };
    let failure = first.disposal().expect("original disposal panic");
    assert_eq!(failure.phase(), PreparationPhase::Disposal);
    assert_eq!(failure.kind(), PreparationFailureKind::Panicked);
    assert!(std::error::Error::source(first.as_ref()).is_none(), "retired disposal receipt cannot own its destroyed panic payload");
    let PreparationError::Retirement(second) = disposing.close().await.expect_err("same original failed retirement cannot reset")
        else { anyhow::bail!("reobserved original panic type lost"); };
    assert!(Arc::ptr_eq(&first, &second)); assert_eq!(control.count.load(Ordering::SeqCst), 1);
    assert!(supervisor.stop().await.is_err());
    Ok(())
}

#[tokio::test]
async fn resource_owning_returned_error_and_panic_wait_for_last_failure_lease_and_native_disposal() -> Result<()> {
    for panic_payload in [false, true] {
        let dir = root(if panic_payload { "resource-panic" } else { "resource-error" })?;
        let cutoff = tokio::time::Instant::now() + Duration::from_secs(15);
        let supervisor = Arc::new(crate::Supervisor::new(shenko_types::ids::RuntimeGeneration("resource-failure".into())));
        let withdrawals = Arc::new(AtomicUsize::new(0));
        supervisor.spawn(DependencyPlugin, withdrawals.clone()).await?;
        let owner = ContextPreparation::<()>::attach_to(&supervisor).await?;
        let control = DropControl::new(false);
        let native_control = control.clone();
        let marker = EntityId::new();
        owner.reserve_start()?.start(move |_| -> Result<()> {
            let resource = ResourceFailure { marker,
                _resource: product(dir.join("owned-failure"), native_control, None)?,
                source: std::io::Error::other("actual nested typed source") };
            if panic_payload { std::panic::panic_any(resource); }
            Err(resource.into())
        })?;
        let delivered = owner.deliver().await.expect_err("actual owned failure must not deliver success");
        let PreparationError::Work(receipt) = &delivered else { anyhow::bail!("nonowning original work receipt missing"); };
        assert!(std::error::Error::source(&delivered).is_none());
        let first = owner.observe_failure().await?.ok_or_else(|| anyhow::anyhow!("original failure lease missing"))?;
        let last = owner.observe_failure().await?.ok_or_else(|| anyhow::anyhow!("second original failure lease missing"))?;
        assert!(std::ptr::eq::<PreparationFailure>(&*first, &*last), "observations must borrow the same original object");
        assert_eq!(first.failure_id(), receipt.failure_id());
        if panic_payload {
            assert!(first.join_error().is_some_and(JoinError::is_panic));
            assert_eq!(first.kind(), PreparationFailureKind::Panicked);
        } else {
            let actual = first.returned_error().and_then(|error| error.downcast_ref::<ResourceFailure>())
                .ok_or_else(|| anyhow::anyhow!("actual resource-owning error type was flattened"))?;
            assert_eq!(actual.marker, marker);
            assert!(std::error::Error::source(actual).is_some_and(|source| source.downcast_ref::<std::io::Error>().is_some()));
        }
        supervisor.hooks.await_current_quiescence().await;
        let ticket = supervisor.plugins.begin_stop()?;
        tokio::time::timeout_at(cutoff, owner.state.cancellation.cancelled()).await?;
        assert!(supervisor.context().try_service::<Dependency>().is_ok());
        assert!(control.gate.thread.lock().is_none(), "a live failure lease must keep its actual payload intact");
        drop(first);
        assert!(control.gate.thread.lock().is_none(), "the last failure lease still owns a borrow");
        let first_observer = supervisor.clone(); let first_ticket = ticket.clone();
        let observing = tokio::spawn(async move { first_observer.plugins.wait_stopped(&first_ticket).await });
        drop(last);
        tokio::time::timeout_at(cutoff, control.gate.entered.notified()).await?;
        assert_ne!(*control.gate.thread.lock(), Some(std::thread::current().id()));
        assert_eq!(control.count.load(Ordering::SeqCst), 0);
        assert_eq!(withdrawals.load(Ordering::SeqCst), 0);
        assert!(supervisor.context().try_service::<Dependency>().is_ok(), "last-lease release must not bypass actual error/panic payload destruction");
        observing.abort(); assert!(observing.await.expect_err("external stop observer cancelled during actual payload disposal").is_cancelled());
        let waiting = supervisor.clone();
        let reobserving = tokio::spawn(async move { waiting.plugins.wait_stopped(&ticket).await });
        assert_eq!(control.count.load(Ordering::SeqCst), 0);
        control.gate.release();
        assert!(tokio::time::timeout_at(cutoff, reobserving).await??.is_err(), "historical native work failure must remain a failed shutdown receipt");
        assert_eq!(control.count.load(Ordering::SeqCst), 1);
        assert_eq!(withdrawals.load(Ordering::SeqCst), 1);
        let completed = owner.close().await.expect_err("original failure receipt survives physical disposal");
        let PreparationError::Retirement(summary) = &completed else { anyhow::bail!("typed retired work receipt missing"); };
        assert_eq!(summary.work().expect("original work identity").failure_id(), receipt.failure_id());
        assert!(std::error::Error::source(summary.as_ref()).is_none());
        let repeated = owner.close().await.expect_err("repeated close cannot reset failed retirement");
        let PreparationError::Retirement(repeated) = repeated else { anyhow::bail!("reobserved receipt missing"); };
        assert!(Arc::ptr_eq(summary, &repeated));
        drop(owner); drop(completed); drop(repeated); drop(delivered);
        assert_eq!(control.count.load(Ordering::SeqCst), 1, "dropping cached receipts on the reactor must own no native payload");
    }
    Ok(())
}

#[tokio::test]
async fn resource_owning_disposal_panic_payload_keeps_original_join_pending_before_dependency_withdrawal() -> Result<()> {
    let dir = root("resource-disposal-panic")?;
    let cutoff = tokio::time::Instant::now() + Duration::from_secs(15);
    let supervisor = Arc::new(crate::Supervisor::new(shenko_types::ids::RuntimeGeneration("resource-disposal-failure".into())));
    let withdrawals = Arc::new(AtomicUsize::new(0));
    supervisor.spawn(DependencyPlugin, withdrawals.clone()).await?;
    let owner = ContextPreparation::<ProductWithResourcePanic>::attach_to(&supervisor).await?;
    let first = DropControl::new(true); let first_native = first.clone();
    let payload = DropControl::new(false); let native_payload = payload.clone();
    payload.panic_after_close.store(true, Ordering::SeqCst);
    owner.reserve_start()?.start(move |_| Ok(ProductWithResourcePanic {
        product: Some(product(dir.join("actual-product"), first_native, None)?),
        payload: Some(ResourceFailure { marker: EntityId::new(),
            _resource: product(dir.join("actual-disposal-panic-payload"), native_payload, None)?,
            source: std::io::Error::other("actual destructor panic source") }),
    }))?;
    drop(owner.deliver().await?);
    supervisor.hooks.await_current_quiescence().await;
    let ticket = supervisor.plugins.begin_stop()?;
    tokio::time::timeout_at(cutoff, payload.gate.entered.notified()).await?;
    assert_eq!(first.count.load(Ordering::SeqCst), 1);
    assert_eq!(payload.count.load(Ordering::SeqCst), 0);
    assert_ne!(*payload.gate.thread.lock(), Some(std::thread::current().id()));
    assert!(supervisor.context().try_service::<Dependency>().is_ok());
    assert_eq!(withdrawals.load(Ordering::SeqCst), 0);
    let observing = supervisor.clone(); let first_ticket = ticket.clone();
    let observation = tokio::spawn(async move { observing.plugins.wait_stopped(&first_ticket).await });
    tokio::task::yield_now().await;
    observation.abort(); assert!(observation.await.expect_err("disposal-failure observer cancelled").is_cancelled());
    assert!(owner.retirement.try_lock().is_err(), "the original native close must still own its retained blocking payload-disposal join");
    let observing = supervisor.clone();
    let completion = tokio::spawn(async move { observing.plugins.wait_stopped(&ticket).await });
    assert!(supervisor.context().try_service::<Dependency>().is_ok());
    payload.gate.release();
    assert!(tokio::time::timeout_at(cutoff, completion).await??.is_err());
    let error = owner.close().await.expect_err("physical cleanup completion must retain historical destructor failure");
    let PreparationError::Retirement(receipt) = &error else { anyhow::bail!("disposal receipt missing"); };
    assert!(receipt.work().is_none());
    assert_eq!(receipt.disposals().len(), 2, "both the original destructor panic and its distinct panic-payload destructor failure must survive as receipts");
    assert_ne!(receipt.disposals()[0].failure_id(), receipt.disposals()[1].failure_id());
    assert!(receipt.disposals().iter().all(|failure|
        failure.phase() == PreparationPhase::Disposal && failure.kind() == PreparationFailureKind::Panicked));
    assert_eq!(receipt.disposal().expect("original destructor panic").phase(), PreparationPhase::Disposal);
    assert_eq!(receipt.disposal().expect("original destructor panic").kind(), PreparationFailureKind::Panicked);
    assert_eq!(payload.count.load(Ordering::SeqCst), 1);
    assert_eq!(withdrawals.load(Ordering::SeqCst), 1);
    assert!(std::error::Error::source(receipt.as_ref()).is_none());
    let repeated = owner.close().await.expect_err("same completed failure receipt must be retained");
    let PreparationError::Retirement(repeated) = repeated else { anyhow::bail!("repeated disposal receipt missing"); };
    assert!(Arc::ptr_eq(receipt, &repeated));
    drop(owner); drop(error); drop(repeated);
    assert_eq!(payload.count.load(Ordering::SeqCst), 1);
    Ok(())
}
