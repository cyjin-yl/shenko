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
use cordis_core::lifecycle::{UpdateError, UpdateOutcome};
use cordis_core::{Context, FiberHandle, Plugin, PreparedChange, PreparedPlugin};
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::errors::{ErrorObservation, ErrorSite};

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
    stopping: Arc<AtomicBool>,
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
        Self {
            context,
            shutdown,
            stopping: Arc::new(AtomicBool::new(false)),
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
        let (send, receive) = oneshot::channel();
        self.context.spawn_attributed(async move {
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
            let _ = send.send(result);
        });
        receive
            .await
            .context("plugin creation owner lost; outcome unknown")?
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
    pub async fn uninstall(&self, fiber: &FiberHandle) -> Result<()> {
        anyhow::ensure!(!self.shutdown.is_cancelled(), "plugin host is shut down");
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
    ) -> Result<UpdateOutcome, UpdateError> {
        anyhow::ensure!(!self.shutdown.is_cancelled(), "plugin host is shut down");
        self.check_outcomes()?;
        let outcome = fiber.update(PreparedChange::from_input(input)).await?;
        self.check_outcomes()?;
        Ok(outcome)
    }

    /// Terminally close admission and await every native resident Fiber.
    /// A concurrent/reentrant stop refuses immediately, rather than waiting on
    /// the very lifecycle callback that must return for shutdown to finish.
    /// Caller cancellation abandons only the wait. Retry after an in-progress or
    /// recursive refusal to observe completion; errors never clear the unknown latch.
    pub async fn stop(&self) -> Result<()> {
        anyhow::ensure!(
            self.stopping
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "plugin shutdown already in progress; do not wait from a lifecycle callback"
        );
        self.shutdown.cancel();
        let context = self.context.clone();
        let stopping = self.stopping.clone();
        let diagnostics = self.diagnostics.clone();
        let (send, receive) = oneshot::channel();
        self.context.spawn_attributed(async move {
            // Cordis closes attach and freezes its flat resident authority in
            // one critical section, including native Context compositions and
            // old allocations already detached by a cancelled typed removal.
            let result = context
                .close_plugins()
                .await
                .map_err(Into::into)
                .and_then(|()| check_outcomes(&diagnostics));
            stopping.store(false, Ordering::SeqCst);
            let _ = send.send(result);
        });
        receive
            .await
            .context("plugin shutdown owner lost; cleanup outcome unknown")?
    }
}

fn check_outcomes(diagnostics: &Diagnostics) -> Result<()> {
    anyhow::ensure!(
        !diagnostics.unknown.load(Ordering::SeqCst),
        "Cordis reported a lifecycle failure; reconcile external outcomes before reusing this host (see diagnostics)"
    );
    Ok(())
}
