//! The memory worker — a real resident service that drains the durable
//! memory intent queue and keeps the Git knowledge repository in sync.
//!
//! SPEC §12.2: accepted ≠ remembered. Acceptance makes the request durable;
//! this worker turns accepted requests into committed memory entries and
//! records the outcome. It is a bounded background worker: it never runs
//! under the ledger mutation line, never blocks the subject loop, and its
//! shutdown is owned by the Cordis lifecycle (SPEC §45, Cordis shutdown
//! barrier).
use std::sync::Arc;
use std::time::Duration;

use cordis_core::{Context, Plugin};
use shenko_store::Store;
use shenko_types::ids::EntityId;
use tokio_util::sync::CancellationToken;

use crate::backend::{FlushOutcome, MemoryBackend, StoreMemoryBackend};
use crate::knowledge::GitKnowledgeStore;

/// How often the worker reconciles the durable pending queue when nothing
/// wakes it earlier. Memory work is not latency-critical; the subject's own
/// saves settle synchronously on its dedicated path.
const DRAIN_INTERVAL: Duration = Duration::from_secs(5);

/// The resident memory worker service.
pub struct MemoryWorker {
    backend: Arc<StoreMemoryBackend>,
    stop: CancellationToken,
}

impl MemoryWorker {
    pub fn new(
        agent_id: EntityId,
        store: Arc<Store>,
        knowledge: Option<Arc<GitKnowledgeStore>>,
    ) -> Self {
        let mut backend = StoreMemoryBackend::new(agent_id, Arc::clone(&store));
        if let Some(knowledge) = knowledge {
            backend = backend.with_knowledge(knowledge);
        }
        Self {
            backend: Arc::new(backend),
            stop: CancellationToken::new(),
        }
    }

    /// The backend this worker drains. Other services (native handlers, the
    /// data service) share it rather than opening a second memory surface.
    pub fn backend(&self) -> Arc<StoreMemoryBackend> {
        Arc::clone(&self.backend)
    }

    /// Reconcile the durable pending queue once. Returns the flush outcome.
    pub fn flush(&self) -> Result<FlushOutcome, anyhow::Error> {
        self.backend.flush()
    }

    /// Run the drain loop until the token is cancelled.
    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        let mut ticker = tokio::time::interval(DRAIN_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = ticker.tick() => {
                    if let Err(error) = self.flush() {
                        tracing::warn!(error = %error, "memory worker flush failed");
                    }
                }
            }
        }
    }

    /// Cancel the resident drain loop. The Cordis shutdown barrier (SPEC §45)
    /// owns when this fires; the loop exits at its next tick boundary.
    pub fn shutdown(&self) {
        self.stop.cancel();
    }
}

/// Cordis plugin wiring for the memory worker.
pub struct MemoryWorkerPlugin;

impl Plugin for MemoryWorkerPlugin {
    type Config = (EntityId, Arc<Store>, Option<Arc<GitKnowledgeStore>>);
    type Input = Arc<MemoryWorker>;
    type PrepareError = std::convert::Infallible;
    type ApplyError = cordis_core::service::ServicePublishError;

    fn name(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("shenko-memory-worker")
    }

    fn prepare(&self, (agent_id, store, knowledge): Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(Arc::new(MemoryWorker::new(agent_id, store, knowledge)))
    }

    async fn apply(&self, ctx: Context, worker: &Self::Input) -> Result<(), Self::ApplyError> {
        let _publication = ctx.provide(worker.clone())?;
        let runner = Arc::clone(worker);
        let stop = worker.stop.clone();
        tokio::spawn(async move { runner.run(stop).await });
        Ok(())
    }
}
