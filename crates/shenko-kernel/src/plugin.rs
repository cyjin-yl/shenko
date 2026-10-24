//! Native Cordis admission and terminal shutdown; no second Plugin/Service contract.
//!
//! Preparation and sealing use the upstream Plugin contract. Cordis owns resident
//! Fibers, generation journals, dependency convergence and panic containment. The
//! only Shenko policy here is terminal admission, retaining unfinished creation
//! independently of its caller, and attempting every typed allocation at shutdown.
//! No graph mutex is held while polling plugin code. Attributed tasks retain
//! Cordis's self-wait detection across our completion-owner task boundary.
use std::any::TypeId;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result};
use cordis_core::logger::{Exporter, Level, LogRecord};
use cordis_core::{Context, FiberHandle, Plugin, PreparedPlugin};
use futures::future::{BoxFuture, join_all};
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

type Removal = Arc<dyn Fn(Context) -> BoxFuture<'static, Result<()>> + Send + Sync>;

#[derive(Default)]
struct Admissions {
    removals: Vec<(TypeId, Removal)>,
    stopping: bool,
}

#[derive(Default)]
struct Diagnostics {
    unknown: AtomicBool,
    records: Mutex<Vec<LogRecord>>,
}

impl Exporter for Diagnostics {
    fn export(&self, record: &LogRecord) {
        // Pinned Cordis reports drain/task failures diagnostically, NOT through
        // dispose/remove's return value. Observe already-contained owned records,
        // never call an arbitrary error's Display or panic payload's destructor.
        if record.level() >= Level::Warn && record.text().starts_with("cordis:") {
            self.unknown.store(true, Ordering::SeqCst);
            self.records.lock().push(record.clone());
        }
    }
}

pub struct PluginRegistry {
    context: Context,
    shutdown: CancellationToken,
    admissions: Arc<Mutex<Admissions>>,
    diagnostics: Arc<Diagnostics>,
}

impl PluginRegistry {
    pub fn new(shutdown: CancellationToken) -> Self {
        let context = Context::new();
        let diagnostics = Arc::new(Diagnostics::default());
        context
            .add_exporter(diagnostics.clone())
            .expect("fresh Cordis root is active");
        Self {
            context,
            shutdown,
            admissions: Arc::new(Mutex::new(Admissions::default())),
            diagnostics,
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
        {
            let mut admissions = self.admissions.lock();
            anyhow::ensure!(!self.shutdown.is_cancelled(), "plugin host is shut down");
            self.check_outcomes()?;
            let key = TypeId::of::<P>();
            if !admissions.removals.iter().any(|(id, _)| *id == key) {
                admissions.removals.push((
                    key,
                    Arc::new(|ctx| {
                        Box::pin(async move { ctx.remove_plugins::<P>().await.map_err(Into::into) })
                    }),
                ));
            }
        }
        let context = self.context.clone();
        let shutdown = self.shutdown.clone();
        let diagnostics = self.diagnostics.clone();
        let (send, receive) = oneshot::channel();
        self.context.spawn_attributed(async move {
            // This definitive admission check is inside the completion owner.
            // An admitted-but-not-yet-polled creation cannot allocate after stop.
            let result = tokio::select! {
                biased;
                _ = shutdown.cancelled() => Err(anyhow::anyhow!("plugin creation interrupted by shutdown")),
                result = context.spawn(prepared) => result.map_err(Into::into),
            };
            // A dropped spawn future delegates rollback to Cordis. The shutdown
            // owner's typed remove waits for that exact native terminal barrier.
            let result = result.and_then(|fiber| {
                check_outcomes(&diagnostics)?;
                Ok(fiber)
            });
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

    /// Terminally close admission and attempt every native typed allocation.
    /// A concurrent/reentrant stop refuses immediately, rather than waiting on
    /// the very lifecycle callback that must return for shutdown to finish.
    /// Caller cancellation abandons only the wait. Retry after an in-progress or
    /// recursive refusal to observe completion; errors never clear the unknown latch.
    pub async fn stop(&self) -> Result<()> {
        let removals = {
            let mut admissions = self.admissions.lock();
            anyhow::ensure!(
                !admissions.stopping,
                "plugin shutdown already in progress; do not wait from a lifecycle callback"
            );
            self.shutdown.cancel();
            admissions.stopping = true;
            admissions
                .removals
                .iter()
                .rev()
                .map(|(_, remove)| remove.clone())
                .collect::<Vec<_>>()
        };
        let context = self.context.clone();
        let admissions = self.admissions.clone();
        let diagnostics = self.diagnostics.clone();
        let (send, receive) = oneshot::channel();
        self.context.spawn_attributed(async move {
            // Poll all independent type removals; a failed/recursive member cannot
            // suppress the rest. Cordis, not a Shenko actor, owns each disposal.
            let results =
                join_all(removals.into_iter().map(|remove| remove(context.clone()))).await;
            let result = results
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .map(|_| ())
                .and_then(|()| check_outcomes(&diagnostics));
            admissions.lock().stopping = false;
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
