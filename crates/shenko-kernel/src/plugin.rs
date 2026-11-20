//! Native Cordis admission and terminal shutdown; no second Plugin/Service contract.
//!
//! Preparation and sealing use the upstream Plugin contract. Cordis owns resident
//! Fibers, generation journals, dependency convergence and panic containment. The
//! only Shenko policy here is terminal admission, retaining unfinished creation
//! independently of its caller, and awaiting native Runtime shutdown.
//! No graph mutex is held while polling plugin code. Attributed tasks retain
//! Cordis's self-wait detection across our completion-owner task boundary.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result};
use cordis_core::logger::{Exporter, Level, LogRecord};
use cordis_core::lifecycle::UpdateOutcome;
use cordis_core::{Context, FiberHandle, Plugin, PreparedChange, PreparedPlugin};
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::errors::{ErrorObservation, ErrorSite};
use crate::preparation::{PreparationAdmission, PreparationScope};

#[derive(Default)]
struct Stop {
    owner: Option<JoinHandle<Result<()>>>,
    outcome: Option<std::result::Result<(), Arc<anyhow::Error>>>,
}

/// Capability held by an external shutdown observer. Only begin_stop can
/// produce it, before shutdown starts awaiting participating scopes. Keep this
/// ticket outside those scopes; waiting for it from their cleanup self-waits.
#[derive(Clone)]
pub struct StopObservation {
    completion: Arc<StopCompletion>,
}
struct StopCompletion {
    state: AsyncMutex<Stop>,
    retryable_refusal: AtomicBool,
    registry: Arc<AtomicBool>,
}

#[derive(Debug)]
struct StopFailure(Arc<anyhow::Error>);
impl std::fmt::Display for StopFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { std::fmt::Display::fmt(&self.0, f) }
}
impl std::error::Error for StopFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.0.as_ref().as_ref()) }
}

#[derive(Default)]
struct Diagnostics {
    unknown: AtomicBool,
    records: Mutex<Vec<LogRecord>>,
    errors: Option<Arc<ErrorObservation>>,
}

impl Exporter for Diagnostics {
    fn export(&self, record: &LogRecord) {
        // Pinned Cordis reports drain/task failures diagnostically, NOT through
        // dispose/remove's return value. Observe already-contained owned records,
        // never call an arbitrary error's Display or panic payload's destructor.
        if record.level() >= Level::Warn && record.text().starts_with("cordis:") {
            self.records.lock().push(record.clone());
            // One journal entry per incident: the false→true transition is the
            // actual unable-to-continue boundary (check_outcomes then refuses
            // every admission), not every subsequent diagnostic record.
            let was_unknown = self.unknown.swap(true, Ordering::SeqCst);
            if !was_unknown {
                if let Some(errors) = &self.errors {
                    errors.observe(
                        ErrorSite::new("cordis_lifecycle", module_path!()),
                        format!("Cordis lifecycle failure: {}", record.text()),
                        serde_json::json!({ "record_level": format!("{:?}", record.level()) }),
                    );
                }
            }
        }
    }
}

pub struct PluginRegistry {
    context: Context,
    shutdown: CancellationToken,
    stop: Mutex<Option<StopObservation>>,
    stopping: Arc<AtomicBool>,
    preparation: Arc<PreparationAdmission>,
    diagnostics: Arc<Diagnostics>,
    errors: Arc<ErrorObservation>,
}

impl PluginRegistry {
    /// Standalone registry with its own private observation point.
    pub fn new(shutdown: CancellationToken) -> Self {
        Self::new_with_errors(shutdown, Arc::new(ErrorObservation::new()))
    }

    /// Registry wired into the assembly's shared observation point, so a
    /// lifecycle incident observed here is the same journal entry the hook
    /// graph and the rest of the kernel report to.
    pub fn new_with_errors(shutdown: CancellationToken, errors: Arc<ErrorObservation>) -> Self {
        let context = Context::new();
        let diagnostics = Arc::new(Diagnostics {
            errors: Some(Arc::clone(&errors)),
            ..Diagnostics::default()
        });
        context
            .add_exporter(diagnostics.clone())
            .expect("fresh Cordis root is active");
        let preparation = PreparationAdmission::new(shutdown.clone());
        context.provide(preparation.clone()).expect("fresh Cordis root admits preparation gate");
        Self {
            context,
            shutdown,
            stop: Mutex::new(None),
            stopping: Arc::new(AtomicBool::new(false)),
            preparation,
            diagnostics,
            errors,
        }
    }

    /// Native typed lookup/observation view. Registrations belong in Plugin::apply,
    /// not on this permanently active root. This is not a second service locator.
    pub fn context(&self) -> &Context {
        &self.context
    }

    pub fn diagnostics(&self) -> Vec<LogRecord> {
        self.diagnostics.records.lock().clone()
    }

    fn check_outcomes(&self) -> Result<()> {
        check_outcomes(&self.diagnostics)
    }

    /// Prepare, seal and admit one real Cordis Plugin. Concurrent calls are distinct
    /// installs, not idempotent aliases. Retain its native FiberHandle for restart.
    /// Dropping this caller's wait does not cancel the admitted creation transaction.
    pub async fn spawn<P: Plugin>(&self, plugin: P, config: P::Config) -> Result<FiberHandle> {
        // Both calls are synchronous native pre-admission user code. A preparation,
        // name or inject panic unwinds only this caller; it cannot kill an owner or
        // lose any existing Fiber. No effects are permitted in preparation.
        let input = plugin
            .prepare(config)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let prepared = PreparedPlugin::from_input(plugin, input);
        anyhow::ensure!(!self.shutdown.is_cancelled(), "plugin host is shut down");
        self.check_outcomes()?;
        let context = self.context.clone();
        let shutdown = self.shutdown.clone();
        let owner = self.context.spawn_attributed(async move {
            // Cancellation interrupts pending apply; the native Runtime gate,
            // not this select poll, linearizes attach against terminal shutdown.
            let result = tokio::select! {
                biased;
                _ = shutdown.cancelled() => Err(anyhow::anyhow!("plugin creation interrupted by shutdown")),
                result = context.spawn(prepared) => result.map_err(Into::into),
            };
            // Return this creation's actual result, even if another Fiber has
            // since reported unknown cleanup. The caller must receive the live
            // handle; host health only gates subsequent admission/restart.
            result
        });
        // Attributed tasks are user-owned. Join the actual owner, so admission
        // does not report completion while its future still retains Context.
        // Dropping this wait detaches the task; committed creation continues.
        owner.await.context("plugin creation owner lost; outcome unknown")?
    }

    /// Destructive same-Fiber restart, not atomic graph activation/handoff.
    /// Native recursion checks remain in the caller's settle context.
    pub async fn restart(&self, fiber: &FiberHandle) -> Result<()> {
        anyhow::ensure!(!self.shutdown.is_cancelled(), "plugin host is shut down");
        self.check_outcomes()?;
        fiber.restart().await?;
        self.check_outcomes()
    }

    /// Terminally end one installed plugin and await its real cleanup barrier.
    ///
    /// The barrier is Cordis's own: the plugin's own generation-owned effects
    /// run — a published service is withdrawn, a registered provider or
    /// capability is removed from the live runtime registry, a spawned worker
    /// is joined — before this call returns. An uninstall therefore cannot
    /// leave a service, a registration or a timer behind, and it cannot claim
    /// completion before the resource is actually released.
    ///
    /// A call from the plugin's own settle context is refused by Cordis
    /// rather than deadlocking on its own teardown.
    /// Closing admission does not close this cleanup path: a resident
    /// watcher may retire its original Fibers after the shutdown token is
    /// cancelled. Their native barriers and diagnostic checks still apply.
    pub async fn uninstall(&self, fiber: &FiberHandle) -> Result<()> {
        fiber
            .dispose()
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        self.check_outcomes()
    }

    /// Replace the committed input of one installed plugin without changing
    /// its identity.
    ///
    /// This is the real hot-update path of the plugin backbone: the same
    /// Fiber keeps its identity, its dependencies and its in-flight work, and
    /// the new input becomes the authoritative generation. A vetoed update
    /// keeps the previous input in force.
    pub async fn update<P: Plugin>(
        &self,
        fiber: &FiberHandle,
        input: P::Input,
    ) -> Result<UpdateOutcome> {
        anyhow::ensure!(!self.shutdown.is_cancelled(), "plugin host is shut down");
        self.check_outcomes()?;
        let outcome = fiber.update(PreparedChange::from_input::<P>(input)).await?;
        self.check_outcomes()?;
        Ok(outcome)
    }

    /// Begin the original terminal shutdown and return its external observation
    /// capability before awaiting any cleanup. Active/reentrant calls refuse,
    /// so a participating cleanup cannot obtain the ticket by calling this.
    pub fn begin_stop(&self) -> Result<StopObservation> {
        anyhow::ensure!(
            self.stopping.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok(),
            "plugin shutdown already in progress; do not wait from a lifecycle callback"
        );
        self.shutdown.cancel();
        let mut stop = self.stop.lock();
        if let Some(observation) = &*stop {
            if !observation.completion.retryable_refusal.load(Ordering::SeqCst) {
                self.stopping.store(false, Ordering::SeqCst);
                return Ok(observation.clone());
            }
        }
        let completion = Arc::new(StopCompletion {
            state: AsyncMutex::new(Stop::default()),
            retryable_refusal: AtomicBool::new(false),
            registry: self.stopping.clone(),
        });
        let settled = completion.clone();
        let context = self.context.clone();
        let preparation = self.preparation.clone();
        let diagnostics = self.diagnostics.clone();
        let stopping = self.stopping.clone();
        let owner = self.context.spawn_attributed(async move {
            let result = async {
                // Closing the same admission gate waits any already admitted
                // internal scope creation. No new scope can follow typed detach.
                preparation.close().await;
                context.remove_plugins::<PreparationScope>().await?;
                // Cordis contains cleanup errors diagnostically. A completed
                // removal proves physical settlement even after a historical
                // failure; retain its diagnostic when closing remaining Fibers.
                context.close_plugins().await?;
                check_outcomes(&diagnostics)
            }.await;
            // A native recursion refusal is precommit for that native removal.
            // Keep its original ticket/result, while permitting a later external
            // begin_stop to finish retirement after the refusing scope returns.
            settled.retryable_refusal.store(result.as_ref().err().is_some_and(|error|
                error.downcast_ref::<cordis_core::lifecycle::LifecycleRecursion>().is_some()), Ordering::SeqCst);
            stopping.store(false, Ordering::SeqCst);
            result
        });
        completion.state.try_lock().expect("new original stop has no observer").owner = Some(owner);
        let observation = StopObservation { completion };
        *stop = Some(observation.clone());
        Ok(observation)
    }

    /// Observe only the original ticket already held outside participating
    /// cleanup. Multiple ticket clones await the same retained owner/result.
    /// Cancellation abandons this observer, never the original join handle.
    pub async fn wait_stopped(&self, observation: &StopObservation) -> Result<()> {
        anyhow::ensure!(Arc::ptr_eq(&self.stopping, &observation.completion.registry),
            "stop observation belongs to a different plugin registry");
        let mut stop = observation.completion.state.lock().await;
        if stop.outcome.is_none() {
            let result = stop.owner.as_mut().expect("original kernel stop owner").await
                .context("plugin shutdown owner lost; cleanup outcome unknown")
                .and_then(|result| result);
            stop.owner.take();
            stop.outcome = Some(result.map_err(Arc::new));
        }
        stop.outcome.as_ref().expect("settled kernel stop outcome").clone()
            .map_err(|error| StopFailure(error).into())
    }

    /// Terminally stop with the existing active/reentrant refusal. An external
    /// caller needing cancellation/reobservation uses begin_stop + wait_stopped.
    pub async fn stop(&self) -> Result<()> {
        let observation = self.begin_stop()?;
        self.wait_stopped(&observation).await
    }
}

fn check_outcomes(diagnostics: &Diagnostics) -> Result<()> {
    if diagnostics.unknown.load(Ordering::SeqCst) {
        // These are already-contained owned strings from Cordis, not calls
        // into a failing plugin's Display implementation. Keep the first
        // actual failure visible when the host returns a terminal error.
        let first = diagnostics.records.lock().first()
            .map(|record| record.text().to_owned())
            .unwrap_or_else(|| "the original diagnostic was unavailable".into());
        anyhow::bail!(
            "Cordis reported a lifecycle failure; reconcile external outcomes before reusing this host: {first}"
        );
    }
    Ok(())
}
