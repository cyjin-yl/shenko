//! Cordis-owned projection of committed PMA decisions. Preparation runs at
//! startup, after a decision, and on ledger changes; request collection only
//! reads the resulting batches. Accepted S0 remains the consumption authority.

use std::{borrow::Cow, convert::Infallible, sync::Arc, time::Duration};

use cordis_core::{Context, InjectSpec, Plugin, Service};
use parking_lot::Mutex;
use shenko_context_policy::{ContributionError, ContributionProvider, ContributionRegistry, ContributionScope};
use shenko_store::{RuntimeErrorContext, Store};
use shenko_types::ids::EntityId;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{MemoryInterventionRegistry, PmaContributionProvider, ReminderPublisher};

pub struct MemoryInterventionConfig {
    pub store: Arc<Store>,
    pub agent_id: EntityId,
    pub policy: Arc<MemoryInterventionRegistry>,
    /// Stop the owning runtime on a failed projection, not on plugin withdrawal.
    pub failure_cancel: CancellationToken,
}

#[derive(Default)]
struct ProjectionState {
    closed: bool,
    commit_seq: Option<u64>,
    scopes: Vec<ContributionScope>,
    worker: Option<JoinHandle<()>>,
}

pub struct PmaContributionRuntime {
    store: Arc<Store>,
    agent_id: EntityId,
    provider: Arc<PmaContributionProvider>,
    registry: Arc<ContributionRegistry>,
    registration: EntityId,
    stop: CancellationToken,
    // Serializes preparation, publication and closure. Never held across await.
    state: Mutex<ProjectionState>,
}

impl Service for PmaContributionRuntime {
    const NAME: &'static str = "shenko/pma-contribution";
}

impl PmaContributionRuntime {
    fn prepare_scope(&self, scope: &ContributionScope) -> Result<(), ContributionError> {
        let ticket = self.registry.begin_publication(self.registration, scope)?;
        let batch = self.provider.prepare(scope)?;
        self.registry.publish(ticket, batch)
    }

    fn prepare_current(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        if state.closed {
            return Ok(());
        }
        // Read before preparation: a concurrent durable mutation must cause
        // another pass, not be mistaken for part of this prepared snapshot.
        let seq = self.store.current_commit_seq()?;
        if state.commit_seq == Some(seq) {
            return Ok(());
        }
        let (head, _) = self.store.load_subject(self.agent_id)?;
        let mut scopes = self.store.pending_intervention_scopes(self.agent_id, chrono::Utc::now().timestamp_millis())?;
        scopes.push(ContributionScope { scope_id: EntityId(head.branch_id.0), task_id: None });
        scopes.sort_by_key(|scope| (scope.scope_id, scope.task_id));
        scopes.dedup();
        for previous in state.scopes.iter().filter(|previous| !scopes.contains(previous)) {
            let ticket = self.registry.begin_publication(self.registration, previous)?;
            self.registry.publish(ticket, Vec::new())?;
        }
        for scope in &scopes { self.prepare_scope(scope)?; }
        state.scopes = scopes;
        state.commit_seq = Some(seq);
        Ok(())
    }

    async fn run(&self) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! { biased;
                _ = self.stop.cancelled() => return Ok(()),
                _ = tick.tick() => self.prepare_current()?,
            }
        }
    }

    fn report_failure(&self, message: String) {
        self.store.journal_runtime_error(self.agent_id, RuntimeErrorContext {
            kind: "pma_projection".into(),
            source_module: module_path!().into(),
            message,
            step_no: None,
            task_id: None,
            extra: serde_json::json!({}),
        });
    }

    async fn close(&self) {
        self.stop.cancel();
        let worker = {
            let mut state = self.state.lock();
            state.closed = true;
            state.worker.take()
        };
        if let Some(worker) = worker {
            if let Err(error) = worker.await {
                self.report_failure(format!("PMA projection worker did not finish normally: {error}"));
            }
        }
        self.registry.unregister(self.registration);
    }
}

impl ReminderPublisher for PmaContributionRuntime {
    fn refresh(&self, scope: &ContributionScope) -> Result<(), ContributionError> {
        let state = self.state.lock();
        if state.closed {
            return Err(ContributionError::RegistrationGone { registration: self.registration });
        }
        // An on-demand refresh publishes the committed batch for exactly this
        // destination. It does not join the periodic scope set: that set is
        // derived from this agent's own pending decisions, and a scope that is
        // not in it must not be emptied as "no longer pending" — that would
        // wipe a batch another owner is still reading.
        self.prepare_scope(scope)
    }
}

pub struct MemoryInterventionPlugin;

impl Plugin for MemoryInterventionPlugin {
    type Config = MemoryInterventionConfig;
    type Input = MemoryInterventionConfig;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("shenko-memory-intervention")
    }

    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(ContributionRegistry::NAME)
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), Self::ApplyError> {
        let registry = ctx.try_service::<ContributionRegistry>().map_err(std::io::Error::other)?;
        let provider = Arc::new(PmaContributionProvider::new(config.store.clone()));
        let registration = registry.register(provider.clone());
        let runtime = Arc::new(PmaContributionRuntime {
            store: config.store.clone(), agent_id: config.agent_id,
            provider, registry: registry.clone(), registration,
            stop: CancellationToken::new(), state: Mutex::new(ProjectionState::default()),
        });
        let cleanup = runtime.clone();
        // Register the join barrier before spawning. Failed setup cannot
        // strand a registration or detach a worker; every apply gets a new
        // cancellation token, including a same-Fiber restart.
        if let Err(error) = ctx.effect(move || async move { cleanup.close().await }) {
            registry.unregister(registration);
            return Err(std::io::Error::other(error));
        }
        runtime.prepare_current().map_err(std::io::Error::other)?;
        let _policy = ctx.provide(config.policy.clone()).map_err(std::io::Error::other)?;
        let _projection = ctx.provide(runtime.clone()).map_err(std::io::Error::other)?;
        let runner = runtime.clone();
        let failure_cancel = config.failure_cancel.clone();
        let mut state = runtime.state.lock();
        if state.closed {
            return Err(std::io::Error::other("PMA projection closed during setup"));
        }
        state.worker = Some(tokio::spawn(async move {
            // Unexpected task termination also stops the owning runtime.
            let failure_guard = failure_cancel.drop_guard();
            match runner.run().await {
                Ok(()) => { failure_guard.disarm(); }
                Err(error) => {
                    runner.registry.unregister(runner.registration);
                    runner.report_failure(format!("PMA projection failed: {error:#}"));
                }
            }
        }));
        Ok(())
    }
}
