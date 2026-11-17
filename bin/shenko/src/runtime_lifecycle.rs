//! Runtime services, live hook events and primary-error-preserving supervisor teardown.
use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use shenko_identity::IdentityService;

use crate::{Command, cli_dispatch, runtime_authority};

#[derive(Default)]
pub(crate) struct RuntimeState {
    pub(crate) active_authority: Option<Arc<runtime_authority::RuntimeAuthority>>,
    pub(crate) authority_heartbeat: Option<tokio::task::JoinHandle<Result<()>>>,
    pub(crate) model_health_monitor: Option<tokio::task::JoinHandle<()>>,
    pub(crate) startup_pause_applied: bool,
}

pub(crate) async fn run(command: Command, data_dir: PathBuf, startup_pause_applied: &mut bool) -> Result<()> {
    // Reuse the actual published C03 host: one native root, one install.
    let supervisor =
        shenko_kernel::supervisor::Supervisor::new(shenko_types::ids::RuntimeGeneration::default());
    // The runtime's own services first: the live plugin registry and the
    // hook graph. Every plugin below resolves them through the Cordis service
    // lookup, so registration and withdrawal happen on the real lifecycle
    // instead of on a startup snapshot.
    let _runtime_services = supervisor.publish_services().await?;
    let policy: Arc<dyn shenko_identity::IdentityPolicy> =
        Arc::new(shenko_identity::DefaultIdentityPolicy);
    let _identity_plugin = supervisor
        .spawn(shenko_identity::IdentityPlugin, policy)
        .await?;
    let identity_context = supervisor.context().clone();
    let identity_service = identity_context.try_service::<IdentityService>()?;
    // Replaceable quota strategies: the runtime publishes the
    // installed forecaster/router/cadence set so the subject, its finite
    // model children and the intervention worker all decide with one set.
    // The configured default is the replayable baseline; installing
    // another strategy is a library/assembly decision and is reported here
    // rather than silently assumed.
    let quota_strategies = shenko_quota::QuotaStrategies::baseline();
    tracing::info!(
        strategies = ?quota_strategies.describe(),
        "quota strategies installed"
    );
    let _quota_strategy_plugin = supervisor
        .spawn(shenko_subject::QuotaStrategyPlugin, quota_strategies)
        .await?;
    let mut state = RuntimeState::default();
    let result = cli_dispatch::dispatch(
        command, data_dir, &supervisor, &identity_context, &identity_service, &mut state,
    ).await;
    *startup_pause_applied = state.startup_pause_applied;
    if let Some(monitor) = state.model_health_monitor.take() {
        monitor.await?;
    }
    // Start's cancellation guard has stopped observations on every normal or
    // error exit. Join their last blocking write before runtime/owner teardown.
    let result = match state.authority_heartbeat.take() {
        Some(heartbeat) => match heartbeat.await
            .map_err(anyhow::Error::from)
            .and_then(std::convert::identity)
        {
            Ok(()) => result,
            Err(observation) => {
                let observation = runtime_authority::observation_failure(observation);
                Err(match result {
                    Ok(()) => observation,
                    Err(error) => observation.context(format!("runtime also failed: {error:#}")),
                })
            },
        },
        None => result,
    };
    let rebuilding = state.active_authority.as_ref()
        .is_some_and(|authority| authority.recovery_required().is_some())
        || result.as_ref().err().is_some_and(runtime_authority::requires_reconstruction);
    let result = if rebuilding {
        match supervisor.stop().await {
            Ok(()) => result,
            Err(error) => Err(error.context("authority teardown failed; no replacement runtime may start before real workers join")),
        }
    } else {
        match supervisor.stop().await {
            Ok(()) => result,
            Err(cleanup) => match result {
                Ok(()) => Err(cleanup),
                Err(error) => Err(error.context(format!("supervisor cleanup also failed: {cleanup}"))),
            },
        }
    };
    // Classify once, after cleanup can no longer replace a genuine storage
    // failure with an earlier authority-recovery wrapper.
    match state.active_authority {
        Some(authority) => authority.finish_result(result),
        None => result,
    }
}

/// Dispatch the runtime's own changes into the live hook graph.
///
/// The registry publishes a revision whenever a provider, a capability or a
/// help document appears or disappears. The graph dispatches those events at
/// this boundary, so a plugin that subscribed to `runtime.capability_changed`
/// observes the real installation instead of a startup snapshot. The task is
/// event driven: it waits for a revision, it does not poll.
pub(super) fn spawn_runtime_hook_pump(
    supervisor: &shenko_kernel::Supervisor,
    cancel: &tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let mut revisions = supervisor.runtime.changed();
    let hooks = supervisor.hooks.clone();
    let cancel = cancel.clone();
    tokio::spawn(async move {
        loop {
            for dispatch in hooks.drain_pending().await {
                tracing::debug!(
                    point = %dispatch.point,
                    results = dispatch.results.len(),
                    "runtime change dispatched into the hook graph"
                );
            }
            // Wait for the next registry revision or the shutdown signal;
            // the pump never blocks teardown.
            tokio::select! {
                _ = cancel.cancelled() => break,
                changed = revisions.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// store-backed kernel error sink: plugin-runtime failures observed
/// at kernel boundaries (Cordis lifecycle incidents, hook-graph admission
/// refusals, publish revision conflicts) journal into the SAME subject store
/// as every other runtime-error fact — no second DB, no autonomous action.
/// `journal_runtime_error` is fail-soft (it counts a dropped journal write
/// instead of propagating), so a journal failure can never recurse into the
/// observation path that produced it.
pub(super) struct StoreErrorSink {
    pub(super) store: Arc<shenko_store::store::Store>,
    pub(super) agent_id: shenko_types::ids::EntityId,
}

impl shenko_kernel::ErrorSink for StoreErrorSink {
    fn observe(
        &self,
        site: shenko_kernel::ErrorSite,
        message: String,
        extra: serde_json::Value,
    ) {
        self.store.journal_runtime_error(
            self.agent_id,
            shenko_store::RuntimeErrorContext {
                kind: site.kind.to_string(),
                source_module: site.module.to_string(),
                message,
                step_no: site.step_no,
                task_id: site.task_id,
                extra,
            },
        );
    }
}
