//! Process-owned stop observation and real ownership of retired runtimes.
use std::{
    future::Future,
    sync::mpsc,
    task::Poll,
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct StopState {
    requested: bool,
    observation_failure: Option<String>,
}

/// A human stop is a host fact, independent of any incarnation's failure
/// cancellation. In particular, a recovery error cannot erase this fact.
#[derive(Clone)]
pub(crate) struct HostStop {
    state: watch::Sender<StopState>,
}

impl Default for HostStop {
    fn default() -> Self {
        let (state, _) = watch::channel(StopState::default());
        Self { state }
    }
}

impl HostStop {
    pub(crate) fn is_stopping(&self) -> bool {
        let state = self.state.borrow();
        state.requested || state.observation_failure.is_some()
    }

    /// Linearize service admission with observed stop. The closure only
    /// transfers already-prepared task ownership; it must not block or await.
    pub(crate) fn start_if_running<T>(&self, start: impl FnOnce() -> T) -> Option<T> {
        let state = self.state.borrow();
        if state.requested || state.observation_failure.is_some() { return None; }
        let started = start();
        drop(state);
        Some(started)
    }

    pub(crate) fn request(&self) {
        self.state.send_modify(|state| state.requested = true);
        tracing::info!("host shutdown signal observed; no replacement runtime will start");
    }

    fn failed(&self, error: String) {
        self.state.send_modify(|state| {
            state.observation_failure.get_or_insert(error);
        });
    }

    fn observation_result(&self) -> Result<()> {
        match self.state.borrow().observation_failure.clone() {
            Some(error) => Err(anyhow::anyhow!(error)),
            None => Ok(()),
        }
    }

    pub(crate) async fn wait(&self) -> Result<()> {
        let mut changes = self.state.subscribe();
        loop {
            let state = changes.borrow_and_update().clone();
            if let Some(error) = state.observation_failure {
                return Err(anyhow::anyhow!(error));
            }
            if state.requested {
                return Ok(());
            }
            changes.changed().await.context("host stop observation channel closed")?;
        }
    }
}

struct ObserverExit {
    stop: HostStop,
    shutdown: CancellationToken,
}

impl Drop for ObserverExit {
    fn drop(&mut self) {
        // A panic before the normal result path must also wake the active
        // incarnation; joining the observer only at final teardown is too late.
        if !self.shutdown.is_cancelled() && !self.stop.is_stopping() {
            self.stop.failed("host signal observer exited unexpectedly".into());
        }
    }
}

struct SignalObserver {
    shutdown: CancellationToken,
    worker: Option<thread::JoinHandle<Result<()>>>,
}

impl SignalObserver {
    fn start(stop: HostStop) -> Result<Self> {
        let shutdown = CancellationToken::new();
        let observer_shutdown = shutdown.clone();
        let (registered, registration) = mpsc::sync_channel(1);
        let worker = thread::Builder::new().name("shenko-host-signal".into()).spawn(move || {
            let _exit = ObserverExit { stop: stop.clone(), shutdown: observer_shutdown.clone() };
            let result: Result<()> = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()
                    .context("create host signal observer runtime")?;
                runtime.block_on(async {
                    let signal = tokio::signal::ctrl_c();
                    tokio::pin!(signal);
                    // ctrl_c registers on its first poll. Do not start an
                    // incarnation before that real registration has completed.
                    let first = std::future::poll_fn(|cx| Poll::Ready(match signal.as_mut().poll(cx) {
                        Poll::Ready(result) => Some(result),
                        Poll::Pending => None,
                    })).await;
                    if let Some(result) = first {
                        result.context("register host shutdown signal")?;
                        stop.request();
                        registered.send(Ok(())).context("host signal registration receiver closed")?;
                        return Ok(());
                    }
                    registered.send(Ok(())).context("host signal registration receiver closed")?;
                    tokio::select! {
                        biased;
                        result = &mut signal => {
                            result.context("observe host shutdown signal")?;
                            stop.request();
                        }
                        _ = observer_shutdown.cancelled() => {}
                    }
                    Ok(())
                })
            })();
            if let Err(error) = &result {
                let reason = format!("host signal observation failed: {error:#}");
                stop.failed(reason.clone());
                // This is needed only before registration. The bounded channel
                // must not block final join after a completed handshake.
                let _ = registered.try_send(Err(reason));
            }
            result
        }).context("start host signal observer")?;
        let observer = Self { shutdown, worker: Some(worker) };
        registration.recv().context("host signal observer exited before registration")?
            .map_err(anyhow::Error::msg)?;
        Ok(observer)
    }

    fn join(&mut self) -> Result<()> {
        self.shutdown.cancel();
        match self.worker.take() {
            Some(worker) => worker.join().map_err(|_| anyhow::anyhow!("host signal observer panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for SignalObserver {
    fn drop(&mut self) {
        if let Err(error) = self.join() {
            tracing::error!(%error, "host signal observer teardown failed");
        }
    }
}

struct RetiredRuntime {
    complete: mpsc::Receiver<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl RetiredRuntime {
    fn start(runtime: tokio::runtime::Runtime) -> Result<Self> {
        let (completed, complete) = mpsc::channel();
        let worker = thread::Builder::new().name("shenko-runtime-join".into()).spawn(move || {
            // A real Runtime drop joins its blocking workers. A timeout must
            // never detach those workers from installation ownership.
            drop(runtime);
            let _ = completed.send(());
        }).context("retain retired runtime worker ownership")?;
        Ok(Self { complete, worker: Some(worker) })
    }

    fn join(&mut self) -> Result<()> {
        match self.worker.take() {
            Some(worker) => worker.join().map_err(|_| anyhow::anyhow!("retired runtime teardown panicked")),
            None => Ok(()),
        }
    }
}

impl Drop for RetiredRuntime {
    fn drop(&mut self) {
        if let Err(error) = self.join() {
            tracing::error!(%error, "retired runtime teardown failed");
        }
    }
}

pub(crate) struct RuntimeHost {
    pub(crate) stop: HostStop,
    retired: Vec<RetiredRuntime>,
    observer: Option<SignalObserver>,
}

/// Preserve the original typed error while reporting every owned cleanup error.
pub(crate) fn with_cleanup(result: Result<()>, cleanup: Result<()>, label: &str) -> Result<()> {
    match cleanup {
        Ok(()) => result,
        Err(cleanup) => Err(match result {
            Ok(()) => cleanup.context(label.to_owned()),
            Err(error) => error.context(format!("{label} also failed: {cleanup:#}")),
        }),
    }
}

impl RuntimeHost {
    pub(crate) fn new(observe_signal: bool) -> Result<Self> {
        let stop = HostStop::default();
        let observer = if observe_signal { Some(SignalObserver::start(stop.clone())?) } else { None };
        Ok(Self { stop, retired: Vec::new(), observer })
    }

    pub(crate) fn retire(&mut self, runtime: tokio::runtime::Runtime) -> Result<()> {
        let mut retiring = RetiredRuntime::start(runtime)?;
        if matches!(retiring.complete.recv_timeout(Duration::from_secs(2)), Err(mpsc::RecvTimeoutError::Timeout)) {
            self.retired.push(retiring);
        } else {
            retiring.join()?;
        }
        // Retain only workers that are still physically alive. Earlier joins
        // may finish while a later incarnation performs its own work.
        let mut index = 0;
        while index < self.retired.len() {
            if self.retired[index].worker.as_ref().is_some_and(thread::JoinHandle::is_finished) {
                self.retired.remove(index).join()?;
            } else {
                index += 1;
            }
        }
        Ok(())
    }

    fn cleanup(&mut self) -> Result<()> {
        let mut result = Ok(());
        for mut runtime in self.retired.drain(..) {
            result = with_cleanup(result, runtime.join(), "retired runtime teardown");
        }
        // Continue observing a real stop during the last blocking join. Only
        // then may the observer end and the enclosing installation owner drop.
        if let Some(mut observer) = self.observer.take() {
            result = with_cleanup(result, observer.join(), "host signal observer teardown");
        }
        with_cleanup(result, self.stop.observation_result(), "host signal observation")
    }

    pub(crate) fn finish(&mut self, result: Result<()>) -> Result<()> {
        with_cleanup(result, self.cleanup(), "owned host teardown")
    }
}

impl Drop for RuntimeHost {
    fn drop(&mut self) {
        // Also covers Runtime construction errors and unwinding: this owner is
        // declared after the installation guard, so no worker can outlive it.
        if (self.observer.is_some() || !self.retired.is_empty()) && let Err(error) = self.cleanup() {
            tracing::error!(%error, "owned host teardown failed");
        }
    }
}
