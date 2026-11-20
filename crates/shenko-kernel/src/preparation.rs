//! Blocking preparation owned by an original native Cordis generation.
//!
//! Attach before capturing work, reserve a single start permit, start the
//! primitive, then borrow delivered products. The original Context effect
//! closes admission, joins the same worker, waits delivery leases and disposes
//! its product on a blocking worker. There is no timer or global registry.
use std::{fmt, future::Future, ops::Deref, pin::Pin, sync::{Arc, atomic::{AtomicUsize, Ordering}}};
use anyhow::Result;
use cordis_core::{Context, Plugin, PreparedPlugin, Service};
use parking_lot::Mutex;
use shenko_types::ids::EntityId;
use tokio::{runtime::Handle, sync::{Mutex as AsyncMutex, Notify}, task::{JoinError, JoinHandle}};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "preparation_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparationStateError { Closed, NotStarted, AlreadyReserved, AlreadyStarted, NotAttached }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparationPhase { Work, Disposal }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparationFailureKind { ReturnedError, Cancelled, Panicked, UnexpectedCancellation, JoinFailure }

/// Nonowning identity and classification. An original source can only be
/// borrowed through an explicit live PreparationFailureLease before close.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparationFailureReceipt {
    failure_id: EntityId,
    owner_id: EntityId,
    phase: PreparationPhase,
    kind: PreparationFailureKind,
}
impl PreparationFailureReceipt {
    pub fn failure_id(&self) -> EntityId { self.failure_id }
    pub fn owner_id(&self) -> EntityId { self.owner_id }
    pub fn phase(&self) -> PreparationPhase { self.phase }
    pub fn kind(&self) -> PreparationFailureKind { self.kind }
}
impl fmt::Display for PreparationFailureReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "preparation {} failure {} {:?}/{:?}", self.owner_id, self.failure_id, self.phase, self.kind)
    }
}

#[derive(Debug)]
enum FailureCause { Returned(anyhow::Error), Join(JoinError) }

/// Retains the actual native error or JoinError, including its panic payload.
#[derive(Debug)]
pub struct PreparationFailure {
    failure_id: EntityId,
    owner_id: EntityId,
    phase: PreparationPhase,
    kind: PreparationFailureKind,
    cause: FailureCause,
}
impl PreparationFailure {
    pub fn failure_id(&self) -> EntityId { self.failure_id }
    pub fn receipt(&self) -> PreparationFailureReceipt {
        PreparationFailureReceipt { failure_id: self.failure_id, owner_id: self.owner_id, phase: self.phase, kind: self.kind }
    }
    pub fn owner_id(&self) -> EntityId { self.owner_id }
    pub fn phase(&self) -> PreparationPhase { self.phase }
    pub fn kind(&self) -> PreparationFailureKind { self.kind }
    pub fn returned_error(&self) -> Option<&anyhow::Error> {
        match &self.cause { FailureCause::Returned(error) => Some(error), _ => None }
    }
    pub fn join_error(&self) -> Option<&JoinError> {
        match &self.cause { FailureCause::Join(error) => Some(error), _ => None }
    }
    fn returned(owner_id: EntityId, error: anyhow::Error) -> Self {
        let cancelled = error.downcast_ref::<PreparationCancelled>().is_some_and(|cause| cause.owner_id == owner_id);
        Self { failure_id: EntityId::new(), owner_id, phase: PreparationPhase::Work,
            kind: if cancelled { PreparationFailureKind::Cancelled } else { PreparationFailureKind::ReturnedError },
            cause: FailureCause::Returned(error) }
    }
    fn joined(owner_id: EntityId, phase: PreparationPhase, error: JoinError) -> Self {
        let kind = if error.is_panic() { PreparationFailureKind::Panicked }
            else if error.is_cancelled() { PreparationFailureKind::UnexpectedCancellation }
            else { PreparationFailureKind::JoinFailure };
        Self { failure_id: EntityId::new(), owner_id, phase, kind, cause: FailureCause::Join(error) }
    }
}
impl fmt::Display for PreparationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "preparation {} {:?}/{:?}: ", self.owner_id, self.phase, self.kind)?;
        match &self.cause {
            FailureCause::Returned(error) => write!(f, "{error:#}"),
            FailureCause::Join(error) => fmt::Display::fmt(error, f),
        }
    }
}
impl std::error::Error for PreparationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match &self.cause { FailureCause::Returned(error) => error.as_ref(), FailureCause::Join(error) => error })
    }
}

#[derive(Debug)]
pub struct PreparationRetirementFailure {
    owner_id: EntityId,
    work: Option<PreparationFailureReceipt>,
    disposal: Vec<PreparationFailureReceipt>,
}
impl PreparationRetirementFailure {
    pub fn owner_id(&self) -> EntityId { self.owner_id }
    pub fn work(&self) -> Option<&PreparationFailureReceipt> { self.work.as_ref() }
    pub fn disposal(&self) -> Option<&PreparationFailureReceipt> { self.disposal.first() }
    pub fn disposals(&self) -> &[PreparationFailureReceipt] { &self.disposal }
}
impl fmt::Display for PreparationRetirementFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "preparation {} retirement failed", self.owner_id)?;
        if let Some(work) = &self.work { write!(f, "; {work}")?; }
        for disposal in &self.disposal { write!(f, "; {disposal}")?; }
        Ok(())
    }
}
impl std::error::Error for PreparationRetirementFailure {}

#[derive(Clone, Debug)]
pub enum PreparationError {
    State { owner_id: EntityId, kind: PreparationStateError },
    Attachment { owner_id: EntityId, cause: Arc<anyhow::Error> },
    Work(PreparationFailureReceipt),
    Retirement(Arc<PreparationRetirementFailure>),
}
impl PreparationError {
    pub fn owner_id(&self) -> EntityId {
        match self {
            Self::State { owner_id, .. } | Self::Attachment { owner_id, .. } => *owner_id,
            Self::Work(failure) => failure.owner_id(), Self::Retirement(failure) => failure.owner_id(),
        }
    }
}
impl fmt::Display for PreparationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::State { owner_id, kind } => write!(f, "preparation {owner_id} {kind:?}"),
            Self::Attachment { owner_id, cause } => write!(f, "preparation {owner_id} attachment failed: {cause:#}"),
            Self::Work(failure) => fmt::Display::fmt(failure, f),
            Self::Retirement(failure) => fmt::Display::fmt(failure, f),
        }
    }
}
impl std::error::Error for PreparationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::State { .. } => None,
            Self::Attachment { cause, .. } => Some(cause.as_ref().as_ref()),
            Self::Work(_) => None, Self::Retirement(failure) => Some(failure.as_ref()),
        }
    }
}

#[derive(Clone)]
pub struct PreparationCancellation { owner_id: EntityId, token: CancellationToken }
impl PreparationCancellation {
    pub fn owner_id(&self) -> EntityId { self.owner_id }
    pub fn is_cancelled(&self) -> bool { self.token.is_cancelled() }
    pub async fn cancelled(&self) { self.token.cancelled().await; }
    pub fn checkpoint(&self) -> Result<()> {
        if self.is_cancelled() { return Err(PreparationCancelled { owner_id: self.owner_id }.into()); }
        Ok(())
    }
}
#[derive(Debug)]
pub struct PreparationCancelled { owner_id: EntityId }
impl PreparationCancelled { pub fn owner_id(&self) -> EntityId { self.owner_id } }
impl fmt::Display for PreparationCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "preparation {} cancelled by original scope", self.owner_id) }
}
impl std::error::Error for PreparationCancelled {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartPhase { Vacant, Reserved, Started }
struct Admission { attached: bool, closed: bool, phase: StartPhase }
enum Work<T> { Empty, Running(JoinHandle<Result<T>>), Ready(Arc<T>), Failed(Arc<PreparationFailure>), Retired }
struct DeliveryLeases { owner_id: EntityId, active: AtomicUsize, changed: Notify }
struct DeliveryLease(Arc<DeliveryLeases>);
impl Drop for DeliveryLease {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::AcqRel) == 1 { self.0.changed.notify_waiters(); }
    }
}
struct PreparationState<T> {
    id: EntityId, runtime: Handle, cancellation: PreparationCancellation,
    admission: Mutex<Admission>, changed: Notify, work: AsyncMutex<Work<T>>, leases: Arc<DeliveryLeases>,
}
type RetirementFuture = Pin<Box<dyn Future<Output = std::result::Result<(), PreparationError>> + Send>>;
#[derive(Default)]
enum Retirement {
    #[default]
    NotStarted,
    Running(RetirementFuture),
    Complete(std::result::Result<(), PreparationError>),
}

/// Original native scope owner. Use attach_to for normal supervisor startup;
/// attach is for an actual resident Plugin::apply Context, not the permanent root.
pub struct ContextPreparation<T: Send + Sync + 'static> {
    state: Arc<PreparationState<T>>,
    retirement: AsyncMutex<Retirement>,
}
impl<T: Send + Sync + 'static> ContextPreparation<T> {
    fn unbound(id: EntityId, cancellation: CancellationToken) -> std::result::Result<Arc<Self>, PreparationError> {
        let runtime = Handle::try_current().map_err(|error| PreparationError::Attachment {
            owner_id: id, cause: Arc::new(error.into()) })?;
        Ok(Arc::new(Self { state: Arc::new(PreparationState {
            id, runtime, cancellation: PreparationCancellation { owner_id: id, token: cancellation },
            admission: Mutex::new(Admission { attached: false, closed: false, phase: StartPhase::Vacant }),
            changed: Notify::new(), work: AsyncMutex::new(Work::Empty),
            leases: Arc::new(DeliveryLeases { owner_id: id, active: AtomicUsize::new(0), changed: Notify::new() }),
        }), retirement: AsyncMutex::new(Retirement::default()) }))
    }
    fn bind(self: &Arc<Self>, ctx: &Context) -> std::result::Result<(), PreparationError> {
        let mut admission = self.state.admission.lock();
        if admission.attached { return Err(self.state_error(PreparationStateError::AlreadyStarted)); }
        let owner = self.clone();
        ctx.effect(move || async move { owner.close().await }).map_err(|error| PreparationError::Attachment {
            owner_id: self.id(), cause: Arc::new(error.into()) })?;
        admission.attached = true;
        Ok(())
    }
    /// Admit a native resident retirement barrier and arm the original
    /// generation effect before any callback captures or work. The kernel
    /// pre-stop barrier closes this admission before withdrawing dependencies.
    pub async fn attach(ctx: &Context) -> std::result::Result<Arc<Self>, PreparationError> {
        Self::attach_inner(ctx, true).await
    }
    async fn attach_inner(ctx: &Context, generation_effect: bool) -> std::result::Result<Arc<Self>, PreparationError> {
        let id = EntityId::new();
        let admission = ctx.try_service::<PreparationAdmission>().map_err(|error| PreparationError::Attachment {
            owner_id: id, cause: Arc::new(error.into()) })?;
        let owner = Self::unbound(id, admission.shutdown.child_token())?;
        let context = ctx.clone();
        // The admission gate stays with the actual native creation owner even
        // when the outer observer is cancelled during an initial settle pass.
        ctx.spawn_attributed(async move {
            let gate = admission.open.lock().await;
            if !*gate || admission.shutdown.is_cancelled() { return Err(owner.state_error(PreparationStateError::Closed)); }
            let retirement: Arc<dyn ScopeRetirement> = owner.clone();
            context.spawn(PreparedPlugin::from_input(PreparationScope, retirement)).await
                .map_err(|error| PreparationError::Attachment { owner_id: id, cause: Arc::new(error.into()) })?;
            if generation_effect {
                let cleanup = owner.clone();
                context.effect(move || async move { cleanup.close().await }).map_err(|error| PreparationError::Attachment {
                    owner_id: id, cause: Arc::new(error.into()) })?;
            }
            drop(gate);
            Ok(owner)
        }).await.map_err(|error| PreparationError::Attachment { owner_id: id, cause: Arc::new(error.into()) })?
    }
    /// A resident native scope makes normal startup part of existing kernel
    /// stop. Permanent supervisor-root effects are intentionally not used.
    pub async fn attach_to(supervisor: &crate::Supervisor) -> std::result::Result<Arc<Self>, PreparationError> {
        Self::attach_inner(supervisor.context(), false).await
    }
    pub fn id(&self) -> EntityId { self.state.id }
    fn state_error(&self, kind: PreparationStateError) -> PreparationError { PreparationError::State { owner_id: self.id(), kind } }
    /// Reserve BEFORE constructing a capturing callback. The non-Clone permit
    /// keeps original cleanup pending; dropping it releases an unused reservation.
    pub fn reserve_start(self: &Arc<Self>) -> std::result::Result<PreparationStart<T>, PreparationError> {
        let mut admission = self.state.admission.lock();
        if !admission.attached { return Err(self.state_error(PreparationStateError::NotAttached)); }
        if admission.closed || self.state.cancellation.is_cancelled() { return Err(self.state_error(PreparationStateError::Closed)); }
        match admission.phase {
            StartPhase::Reserved => return Err(self.state_error(PreparationStateError::AlreadyReserved)),
            StartPhase::Started => return Err(self.state_error(PreparationStateError::AlreadyStarted)),
            StartPhase::Vacant => admission.phase = StartPhase::Reserved,
        }
        Ok(PreparationStart { owner: self.clone(), armed: true })
    }
    pub async fn deliver(&self) -> std::result::Result<PreparedProduct<T>, PreparationError> {
        {
            let admission = self.state.admission.lock();
            if admission.closed || self.state.cancellation.is_cancelled() { return Err(self.state_error(PreparationStateError::Closed)); }
            if admission.phase != StartPhase::Started { return Err(self.state_error(PreparationStateError::NotStarted)); }
        }
        let mut work = self.state.work.lock().await;
        resolve_work(self.id(), &mut work).await;
        if let Work::Failed(failure) = &*work { return Err(PreparationError::Work(failure.receipt())); }
        let admission = self.state.admission.lock();
        if admission.closed || self.state.cancellation.is_cancelled() { return Err(self.state_error(PreparationStateError::Closed)); }
        let Work::Ready(value) = &*work else { return Err(self.state_error(PreparationStateError::NotStarted)); };
        self.state.leases.active.fetch_add(1, Ordering::AcqRel);
        Ok(PreparedProduct { value: value.clone(), _lease: DeliveryLease(self.state.leases.clone()) })
    }
    /// Opt in to inspecting the original work error or panic before retirement.
    /// This is an explicit counted lease, never acquired by ordinary `?`.
    /// Drop every failure lease before awaiting close or kernel stop.
    pub async fn observe_failure(&self) -> std::result::Result<Option<PreparationFailureLease>, PreparationError> {
        {
            let admission = self.state.admission.lock();
            if admission.closed || self.state.cancellation.is_cancelled() { return Err(self.state_error(PreparationStateError::Closed)); }
            if admission.phase != StartPhase::Started { return Err(self.state_error(PreparationStateError::NotStarted)); }
        }
        let mut work = self.state.work.lock().await;
        resolve_work(self.id(), &mut work).await;
        let admission = self.state.admission.lock();
        if admission.closed || self.state.cancellation.is_cancelled() { return Err(self.state_error(PreparationStateError::Closed)); }
        match &*work {
            Work::Failed(failure) => {
                self.state.leases.active.fetch_add(1, Ordering::AcqRel);
                Ok(Some(PreparationFailureLease { failure: failure.clone(), _lease: DeliveryLease(self.state.leases.clone()) }))
            }
            Work::Ready(_) => Ok(None),
            _ => Err(self.state_error(PreparationStateError::NotStarted)),
        }
    }
    /// Close admission synchronously. This is a request, not proof of joining.
    pub fn request_close(&self) {
        self.state.admission.lock().closed = true;
        self.state.cancellation.token.cancel();
        self.state.changed.notify_waiters();
    }
    /// Observe the same retained join/disposal future. Cancellation abandons
    /// only this observer; original Context cleanup still owns the owner.
    /// Drop all PreparedProduct and PreparationFailureLease guards before
    /// awaiting this barrier. Its cached errors are nonowning typed receipts;
    /// original work error/panic payloads are destroyed on the blocking join.
    pub async fn close(&self) -> std::result::Result<(), PreparationError> {
        self.request_close();
        let mut retirement = self.retirement.lock().await;
        loop {
            match &mut *retirement {
                Retirement::NotStarted => {
                    *retirement = Retirement::Running(Box::pin(retire_state(self.state.clone())));
                }
                Retirement::Running(future) => {
                    let outcome = future.await;
                    *retirement = Retirement::Complete(outcome.clone());
                    return outcome;
                }
                Retirement::Complete(outcome) => return outcome.clone(),
            }
        }
    }
}

/// One original preparation admission. No callback exists until this permit
/// has been acquired, so rejection cannot silently destroy caller authorities.
pub struct PreparationStart<T: Send + Sync + 'static> { owner: Arc<ContextPreparation<T>>, armed: bool }
impl<T: Send + Sync + 'static> PreparationStart<T> {
    pub fn start<F>(mut self, work: F) -> std::result::Result<(), PreparationError>
    where F: FnOnce(PreparationCancellation) -> Result<T> + Send + 'static,
    {
        let mut admission = self.owner.state.admission.lock();
        // Reserved has no delivery observer; close waits reservation release
        // before locking this slot. The guard remains synchronous through spawn.
        let Ok(mut slot) = self.owner.state.work.try_lock() else {
            unreachable!("reserved preparation slot has no async observer");
        };
        let closed = admission.closed || self.owner.state.cancellation.is_cancelled();
        let cancellation = self.owner.state.cancellation.clone();
        let task = self.owner.state.runtime.spawn_blocking(move || {
            if let Err(error) = cancellation.checkpoint() { drop(work); return Err(error); }
            match work(cancellation.clone()) {
                Ok(product) => match cancellation.checkpoint() {
                    Ok(()) => Ok(product),
                    Err(error) => { drop(product); Err(error) }
                },
                Err(error) => Err(error),
            }
        });
        *slot = Work::Running(task);
        admission.phase = StartPhase::Started;
        self.armed = false;
        self.owner.state.changed.notify_waiters();
        if closed { Err(self.owner.state_error(PreparationStateError::Closed)) } else { Ok(()) }
    }
}
impl<T: Send + Sync + 'static> Drop for PreparationStart<T> {
    fn drop(&mut self) {
        if self.armed {
            self.owner.state.admission.lock().phase = StartPhase::Vacant;
            self.owner.state.changed.notify_waiters();
        }
    }
}

/// A scoped delivery lease. It exposes a borrow, never the owner's Arc.
/// External aliases cloned from inside T remain the caller's explicit owners
/// and must be withdrawn before original scope cleanup (native LIFO order).
/// Field order releases the product Arc before the counted lease wakes close.
pub struct PreparedProduct<T> { value: Arc<T>, _lease: DeliveryLease }
impl<T> Deref for PreparedProduct<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.value }
}
impl<T> fmt::Debug for PreparedProduct<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.debug_struct("PreparedProduct").field("owner_id", &self._lease.0.owner_id).finish_non_exhaustive() }
}

/// Explicit access to one original typed work failure. The primitive retains
/// its own strong payload until this lease is released, then disposes it on
/// its original retained blocking retirement path. No underlying Arc escapes.
/// Field order releases the failure Arc before the counted lease wakes close.
pub struct PreparationFailureLease { failure: Arc<PreparationFailure>, _lease: DeliveryLease }
impl Deref for PreparationFailureLease {
    type Target = PreparationFailure;
    fn deref(&self) -> &Self::Target { &self.failure }
}
impl fmt::Debug for PreparationFailureLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparationFailureLease").field("receipt", &self.receipt()).finish_non_exhaustive()
    }
}
async fn resolve_work<T: Send + Sync + 'static>(id: EntityId, work: &mut Work<T>) {
    if let Work::Running(task) = work {
        let result = task.await;
        *work = match result {
            Ok(Ok(product)) => Work::Ready(Arc::new(product)),
            Ok(Err(error)) => Work::Failed(Arc::new(PreparationFailure::returned(id, error))),
            Err(error) => Work::Failed(Arc::new(PreparationFailure::joined(id, PreparationPhase::Work, error))),
        };
    }
}

/// Shared extraction of ColdPreparation's completed-product disposal. Callers
/// MUST retain the enclosing future until this actual blocking join completes.
pub async fn dispose_product<T: Send + 'static>(product: T) -> std::result::Result<(), JoinError> {
    tokio::task::spawn_blocking(move || drop(product)).await
}

async fn retire_state<T: Send + Sync + 'static>(state: Arc<PreparationState<T>>) -> std::result::Result<(), PreparationError> {
    loop {
        let changed = state.changed.notified(); tokio::pin!(changed); changed.as_mut().enable();
        if state.admission.lock().phase != StartPhase::Reserved { break; }
        changed.await;
    }
    let mut work = state.work.lock().await;
    resolve_work(state.id, &mut work).await;
    loop {
        let changed = state.leases.changed.notified(); tokio::pin!(changed); changed.as_mut().enable();
        if state.leases.active.load(Ordering::Acquire) == 0 { break; }
        changed.await;
    }
    let (product, failure) = match std::mem::replace(&mut *work, Work::Retired) {
        Work::Ready(product) => (Some(product), None),
        Work::Failed(failure) => (None, Some(failure)),
        Work::Empty => (None, None),
        Work::Retired | Work::Running(_) => unreachable!("original preparation resolves once before retirement"),
    };
    let original = failure.as_ref().filter(|failure| failure.kind() != PreparationFailureKind::Cancelled).map(|failure| failure.receipt());
    let mut disposal = Vec::new();
    if product.is_some() || failure.is_some() {
        let mut pending = dispose_product((product, failure)).await;
        while let Err(error) = pending {
            // A failed destructor's actual JoinError can itself own a native
            // panic payload. It is a new cleanup obligation, not a replay of
            // the failed destructor. Retain its blocking join in this SAME
            // retirement future; never return a resource-owning diagnostic.
            let failure = PreparationFailure::joined(state.id, PreparationPhase::Disposal, error);
            disposal.push(failure.receipt());
            pending = dispose_product(failure).await;
        }
    }
    if original.is_some() || !disposal.is_empty() {
        return Err(PreparationError::Retirement(Arc::new(PreparationRetirementFailure {
            owner_id: state.id, work: original, disposal,
        })));
    }
    Ok(())
}

/// One admission gate on the existing kernel Context, never an owner list.
/// Its only critical section admits the internal native scope, without work.
pub(crate) struct PreparationAdmission {
    open: AsyncMutex<bool>,
    shutdown: CancellationToken,
}
impl PreparationAdmission {
    pub(crate) fn new(shutdown: CancellationToken) -> Arc<Self> {
        Arc::new(Self { open: AsyncMutex::new(true), shutdown })
    }
    pub(crate) async fn close(&self) { *self.open.lock().await = false; }
}
impl Service for PreparationAdmission {
    const NAME: &'static str = "shenko.preparation-admission/v1";
}

pub(crate) trait ScopeRetirement: Send + Sync {
    fn arm(self: Arc<Self>, ctx: &Context) -> std::result::Result<(), PreparationError>;
}
impl<T: Send + Sync + 'static> ScopeRetirement for ContextPreparation<T> {
    fn arm(self: Arc<Self>, ctx: &Context) -> std::result::Result<(), PreparationError> { self.bind(ctx) }
}

/// All product types share this original native registry group. Kernel stop
/// removes and joins this group before it closes the remaining resident graph.
pub(crate) struct PreparationScope;
impl Plugin for PreparationScope {
    type Config = Arc<dyn ScopeRetirement>;
    type Input = Self::Config;
    type PrepareError = std::convert::Infallible;
    type ApplyError = PreparationError;
    fn name(&self) -> std::borrow::Cow<'_, str> { "kernel-owned-preparation".into() }
    fn prepare(&self, owner: Self::Config) -> std::result::Result<Self::Input, Self::PrepareError> { Ok(owner) }
    async fn apply(&self, ctx: Context, owner: &Self::Input) -> std::result::Result<(), Self::ApplyError> {
        owner.clone().arm(&ctx)
    }
}
