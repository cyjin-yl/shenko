//! Installed strategies, prepared windows and window commits, published as
//! one Cordis Service.
//!
//! Each strategy plugin installs itself here from its own `apply` and removes
//! itself in cleanup, so install/update/uninstall/replace are ordinary Cordis
//! lifecycle operations on that plugin alone. The registry holds no strategy.
//!
//! The preparation path is split in two on purpose (docs/specs/context-policies.md#spec-12-3-2):
//!
//! * [`PolicyRegistry::prepare_candidate`] does the slow work — reading the
//!   raw history and rendering the new representation. It runs off the
//!   request path.
//! * [`PolicyRegistry::publish`] binds the result to the live revisions and
//!   [`PolicyRegistry::adopt_prepared`] commits it. The request path only
//!   adopts an already published window; it never prepares one.
use std::borrow::Cow;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use cordis_core::{Context, Plugin, Service};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

use crate::history::{HistoryItem, HistorySource, StorageError};
use crate::policy::{
    Availability, ContextInput, ContextPolicy, CostReport, ModelCaps, PolicyError,
    PolicyManifest, PreparedWindow, TriggerInput, WindowBase, WindowEntry,
};
use crate::preparation::{
    PreparationBinding, PreparationDiagnostic, PreparationOutcome, PreparedCandidate,
    PreferenceAdoption, PressureObservation, PublishError,
};
use crate::window::{CommitError, CommittedWindow, WindowLedger};

struct Installed {
    policy: Arc<dyn ContextPolicy>,
    manifest: PolicyManifest,
    epoch: u64,
}

#[derive(Default)]
struct RegistryState {
    installed: Vec<Installed>,
    preference: Vec<String>,
    next_epoch: u64,
    configuration_revision: u64,
    /// The highest epoch observed in the committed chain; a new installation
    /// starts above it, so a window prepared by an earlier process can never
    /// commit.
    durable_epoch_base: Option<u64>,
    /// The window prepared off the request path and waiting for adoption.
    candidate: Option<PreparedCandidate>,
    /// The last observed input size, reported by the subject.
    pressure: Option<PressureObservation>,
    /// The visible outcome of the last preparation.
    last_preparation: Option<PreparationDiagnostic>,
    /// A request for a new window at the next safe boundary, independent of
    /// the size trigger.
    rollover: Option<RolloverRequest>,
    /// The once-per-window reminder threshold an installed strategy
    /// advertised, bound to its installation epoch.
    reminder: Option<ReminderThreshold>,
    /// Committed windows whose reminder this process already acknowledged, so
    /// one window gets at most one reminder however many steps it spans. The
    /// durable journal, when attached, is the authority across restarts; this
    /// set is the in-process copy that spares a reader per step.
    reminder_issued: std::collections::BTreeSet<EntityId>,
    /// Durable arm/answer of explicit rollover requests, when this process
    /// has a store-backed journal.
    rollover_journal: Option<Arc<dyn crate::notes::RolloverJournal>>,
    /// Durable once-per-window answer of the reminder, when this process has
    /// a store-backed journal. Without it the answer is process-local, which
    /// is the honest scope of a registry that has no store.
    reminder_journal: Option<Arc<dyn crate::notes::ReminderJournal>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ReminderThreshold {
    epoch: u64,
    ratio: f32,
}

/// The bounded reminder text a context near its automatic threshold adds to
/// the per-step prompt suffix. It names capabilities; it does not script the
/// model's reasoning.
pub const ROLLOVER_REMINDER: &str = "本窗口接近自动切分点。若当前工作状态重要，用 context.notebook 的 replace 写入要点（最新版本会进入下一个窗口），然后在需要时用 rollover 在下一个安全边界开始新窗口；原始历史仍可经 history.query 按 seq/event_id 读取。";

/// Facts of one issued reminder, for honest reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloverReminder {
    /// The committed window the reminder belongs to.
    pub window: EntityId,
    pub used_tokens: u32,
    pub max_context: u32,
    /// The installation epoch that advertised the threshold. The
    /// acknowledgement is fenced on it, so a strategy retired or replaced
    /// between preparation and acknowledgement can neither answer the
    /// reminder itself nor have its successor's threshold spent.
    pub policy_epoch: u64,
}

/// A pending request to start a new window at the next safe tool-loop
/// boundary — the installed notebook tool's `new_context` equivalent.
///
/// The request is a live fact of this process and is bound to the branch it
/// was made on. A crash before the boundary loses the request, never the
/// history; the caller is told that it was armed, not that a window exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloverRequest {
    /// Stable identity of this request, so an answer can name the request it
    /// satisfied instead of guessing from timestamps.
    pub request_id: EntityId,
    pub scope_id: EntityId,
    pub requested_at_ms: i64,
}

/// One strategy's outcome while selecting a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub policy: String,
    pub outcome: Result<(), PolicyError>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum PrepareError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("no installed context policy produced a usable window: {attempts:?}")]
    NoUsablePolicy { attempts: Vec<Attempt> },
    #[error("prepared window was refused: {0}")]
    Publish(PublishError),
}

impl PrepareError {
    /// Strategy outcomes, empty when history could not be read at all.
    pub fn attempts(&self) -> &[Attempt] {
        match self {
            Self::Storage(_) => &[],
            Self::NoUsablePolicy { attempts } => attempts,
            Self::Publish(_) => &[],
        }
    }
}

/// Smallest share of the current window a new window must save.
///
/// This is the registry's own guard against re-cutting the same raw history
/// into an equally large window: it costs a full prefill and buys nothing.
/// It is not a strategy decision — when to compact at all belongs to the
/// installed policy.
pub const MIN_GAIN_RATIO: f32 = 0.05;

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|since| since.as_millis() as i64).unwrap_or(0)
}

fn raw_items(history: &dyn HistorySource) -> Result<Vec<HistoryItem>, StorageError> {
    match history.leaf_seq()? {
        Some(leaf) => history.read_range(0, leaf),
        None => Ok(Vec::new()),
    }
}

/// The tokens of the raw events that arrived after `leaf`.
///
/// These events are carried verbatim into the window; their size is part of
/// the capacity the window actually occupies.
fn tail_tokens(history: &dyn HistorySource, leaf: u64) -> Result<u32, StorageError> {
    let Some(current) = history.leaf_seq()? else {
        return Ok(0);
    };
    if current <= leaf {
        return Ok(0);
    }
    Ok(history.read_range(leaf + 1, current)?.iter().map(|item| item.tokens).sum())
}

#[derive(Default)]
pub struct PolicyRegistry {
    state: Mutex<RegistryState>,
    ledger: WindowLedger,
}

impl Service for PolicyRegistry {
    const NAME: &'static str = "shenko/context-policy";
}

impl PolicyRegistry {
    /// Registry over an in-process window log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry whose committed windows live in `ledger`'s log.
    pub fn with_ledger(ledger: WindowLedger) -> Self {
        Self { state: Mutex::default(), ledger }
    }

    pub fn ledger(&self) -> &WindowLedger {
        &self.ledger
    }

    /// Process-local revision of the strategy set and preference order.
    /// A failed preparation only describes the configuration it observed;
    /// callers can retry after replacement without retrying unchanged failures.
    pub fn configuration_revision(&self) -> u64 {
        self.state.lock().configuration_revision
    }

    /// Install or replace the strategy with this manifest id. Returns the new
    /// epoch; windows prepared by any earlier epoch can no longer commit.
    ///
    /// The epoch continues above every epoch the committed chain already
    /// used, so an installation that follows a restart can never adopt a
    /// window that an earlier process prepared.
    pub fn install(&self, policy: Arc<dyn ContextPolicy>) -> Result<u64, StorageError> {
        let manifest = policy.manifest();
        let mut state = self.state.lock();
        let durable = match state.durable_epoch_base {
            Some(base) => base,
            None => {
                // A failed read is not "epoch zero": guessing could reuse an
                // epoch a committed window already carries.
                let base = self.ledger.max_policy_epoch()?;
                state.durable_epoch_base = Some(base);
                base
            }
        };
        state.next_epoch = state.next_epoch.max(durable);
        state.next_epoch += 1;
        let epoch = state.next_epoch;
        state.installed.retain(|i| i.manifest.id != manifest.id);
        state.installed.push(Installed { policy, manifest, epoch });
        state.configuration_revision += 1;
        Ok(epoch)
    }

    /// Remove only that exact installation, so a late cleanup of a replaced
    /// generation cannot uninstall its successor.
    pub fn uninstall(&self, id: &str, epoch: u64) -> bool {
        let mut state = self.state.lock();
        let before = state.installed.len();
        state.installed.retain(|i| !(i.manifest.id == id && i.epoch == epoch));
        let removed = state.installed.len() != before;
        if removed {
            state.configuration_revision += 1;
        }
        removed
    }

    pub fn installed(&self) -> Vec<(PolicyManifest, u64)> {
        let state = self.state.lock();
        state.installed.iter().map(|i| (i.manifest.clone(), i.epoch)).collect()
    }

    pub fn availability(&self, caps: &ModelCaps) -> Vec<(String, Availability)> {
        self.candidates()
            .into_iter()
            .map(|(policy, manifest, _)| (manifest.id, policy.availability(caps)))
            .collect()
    }

    /// Strategy order chosen by the subject or configuration; ids not listed
    /// follow in installation order.
    pub fn set_preference(&self, ids: Vec<String>) {
        let mut state = self.state.lock();
        if state.preference != ids {
            state.preference = ids;
            state.configuration_revision += 1;
        }
    }

    /// The preference order currently in effect, for honest reporting.
    pub fn preference(&self) -> Vec<String> {
        self.state.lock().preference.clone()
    }

    /// Apply a configured order and report what it actually became.
    ///
    /// The requested ids that are installed keep their order; the remaining
    /// installed ids follow. A requested id that is not installed is reported
    /// as unknown instead of being silently dropped.
    pub fn apply_preference(&self, ids: Vec<String>) -> PreferenceAdoption {
        let mut state = self.state.lock();
        if state.preference != ids {
            state.preference = ids.clone();
            state.configuration_revision += 1;
        }
        let installed: Vec<String> =
            state.installed.iter().map(|i| i.manifest.id.clone()).collect();
        let mut adopted = Vec::new();
        let mut unknown = Vec::new();
        for id in &ids {
            if installed.iter().any(|installed| installed == id) {
                if !adopted.contains(id) {
                    adopted.push(id.clone());
                }
            } else {
                unknown.push(id.clone());
            }
        }
        for id in installed {
            if !adopted.contains(&id) {
                adopted.push(id);
            }
        }
        PreferenceAdoption { requested: ids, adopted, unknown, adopted_at_ms: now_ms() }
    }

    /// Whether the first usable strategy wants a new window now.
    ///
    /// The decision reads no history: it depends on the observed input size
    /// and the model's real limits. Reading the history belongs to the
    /// preparation, which runs off the request path.
    pub fn should_compact(&self, caps: &ModelCaps, used_tokens: u32) -> bool {
        let Some((policy, _, _)) = self
            .candidates()
            .into_iter()
            .find(|(p, _, _)| !matches!(p.availability(caps), Availability::Unavailable(_)))
        else {
            return false;
        };
        policy.should_compact(&TriggerInput { caps, used_tokens })
    }

    /// Report the observed input size, so the preparation can be triggered
    /// without reading the history inside the request.
    ///
    /// The observation carries the exact model limits it was made under, so
    /// a preparation that runs later prepares for the model that was really
    /// observed instead of a re-derived guess.
    pub fn observe_pressure(&self, caps: &ModelCaps, used_tokens: u32) -> Result<(), StorageError> {
        self.state.lock().pressure = Some(PressureObservation {
            used_tokens,
            max_context: caps.max_context,
            model_id: caps.model_id.clone(),
            caps: caps.clone(),
            observed_at_ms: now_ms(),
        });
        Ok(())
    }

    /// The last observed input size.
    pub fn pressure(&self) -> Option<PressureObservation> {
        self.state.lock().pressure.clone()
    }

    /// The cost facts of the committed window the model sees now.
    ///
    /// `reusable_prefix_tokens` is a conservative upper bound on the prefix
    /// whose final input is unchanged; `recompute_tokens` is the part that
    /// must be recomputed. Both are input-identity bounds, never a backend
    /// cache-hit measurement — `kv_accounting` says which.
    pub fn current_cost(&self) -> Result<CostReport, StorageError> {
        Ok(self.ledger.current()?.cost)
    }

    /// Whether a window switch is waiting to be adopted.
    ///
    /// This is the signal that the model's input is about to change, so a
    /// residency policy can plan for it. It is a fact about the prepared
    /// candidate, not a prediction.
    pub fn prepared_candidate_pending(&self) -> bool {
        self.state.lock().candidate.is_some()
    }

    /// Whether the committed window on its own already exceeds the model's
    /// context limit.
    ///
    /// It reads the window ledger only — the raw history is never touched —
    /// so it is safe to call on the request path. The figure is the window's
    /// own recorded size; the raw events appended after it was cut are not
    /// counted here, because counting them would mean reading the history.
    /// The caller that knows the real projected size passes it to
    /// [`PolicyRegistry::needs_preparation`].
    pub fn window_over_budget(&self, caps: &ModelCaps) -> Result<bool, StorageError> {
        let window = self.ledger.current()?;
        Ok(window.cost.window_tokens > caps.max_context)
    }

    /// The one cheap decision a request path needs: does a new window have to
    /// be prepared?
    ///
    /// It combines the installed strategy's own trigger with the
    /// ledger-only over-budget check. It reads the window ledger and the
    /// recorded pressure; it never reads the raw history, so it is safe to
    /// call inside a request transaction.
    pub fn needs_preparation(&self, caps: &ModelCaps, used_tokens: u32) -> bool {
        if self.rollover_pending() {
            return true;
        }
        if self.should_compact(caps, used_tokens) {
            return true;
        }
        self.window_over_budget(caps).unwrap_or(false)
    }

    /// Attach the durable rollover journal of this branch.
    ///
    /// The journal makes a request outlive the process that armed it. It must
    /// belong to the same branch as the window chain; a mismatched journal is
    /// refused instead of letting one branch's request cut another's window.
    pub fn attach_rollover_journal(
        &self,
        journal: Arc<dyn crate::notes::RolloverJournal>,
    ) -> Result<(), StorageError> {
        let scope = self.ledger.scope_id();
        if journal.scope_id() != scope {
            return Err(StorageError(format!(
                "rollover journal belongs to scope {}, this context chain is {scope}",
                journal.scope_id()
            )));
        }
        // A request armed before this process started is still pending: the
        // durable row is the authority, so attaching the journal restores the
        // live request instead of waiting for the model to ask again.
        let pending = journal.pending().map_err(|error| StorageError(error.to_string()))?;
        let mut state = self.state.lock();
        state.rollover_journal = Some(journal);
        if let Some(request) = pending {
            if state.rollover.is_none() {
                state.rollover = Some(request);
            }
        }
        Ok(())
    }

    /// Ask for a new window at the next safe boundary even when the size
    /// trigger is false.
    ///
    /// The request belongs to this branch: it is refused when the ledger's
    /// scope is not the scope the caller observed. With a durable journal
    /// attached, the request is committed there before it is remembered live,
    /// so a crash before the boundary still rolls over. A repeated request
    /// before the boundary keeps the first one, so an impatient tool cannot
    /// stack windows.
    pub fn request_rollover(&self, scope_id: EntityId) -> Result<RolloverRequest, StorageError> {
        let current = self.ledger.scope_id();
        if current != scope_id {
            return Err(StorageError(format!(
                "rollover requested for scope {scope_id}, but this context chain is {current}"
            )));
        }
        let durable = {
            let state = self.state.lock();
            state.rollover_journal.clone()
        };
        let request = match durable {
            Some(journal) => journal
                .request(now_ms())
                .map_err(|error| StorageError(error.to_string()))?,
            None => RolloverRequest {
                request_id: EntityId::new(),
                scope_id: current,
                requested_at_ms: now_ms(),
            },
        };
        let mut state = self.state.lock();
        if let Some(existing) = state.rollover.clone() {
            return Ok(existing);
        }
        state.rollover = Some(request.clone());
        Ok(request)
    }

    /// The request waiting for the next safe boundary.
    pub fn rollover_request(&self) -> Option<RolloverRequest> {
        self.state.lock().rollover.clone()
    }

    pub fn rollover_pending(&self) -> bool {
        self.state.lock().rollover.is_some()
    }

    /// Register the once-per-window reminder threshold of an installation.
    ///
    /// The threshold is configuration of the strategy that owns it, so it
    /// belongs to that installation epoch and is removed with it.
    pub fn set_reminder_threshold(&self, epoch: u64, ratio: f32) {
        self.state.lock().reminder = Some(ReminderThreshold { epoch, ratio });
    }

    /// Withdraw the threshold only if this exact installation registered it,
    /// so a late cleanup cannot remove its successor's threshold.
    pub fn clear_reminder_threshold(&self, epoch: u64) {
        let mut state = self.state.lock();
        if state.reminder.is_some_and(|threshold| threshold.epoch == epoch) {
            state.reminder = None;
        }
    }

    /// Attach the durable once-per-window reminder journal of this branch.
    ///
    /// The journal makes "this committed window already received its one
    /// reminder" a fact that outlives the process that issued it, so a restart
    /// inside the same window is silent instead of repeating prompt material
    /// the model already saw. It must belong to the same branch as the window
    /// chain; a mismatched journal is refused.
    pub fn attach_reminder_journal(
        &self,
        journal: Arc<dyn crate::notes::ReminderJournal>,
    ) -> Result<(), StorageError> {
        let scope = self.ledger.scope_id();
        if journal.scope_id() != scope {
            return Err(StorageError(format!(
                "reminder journal belongs to scope {}, this context chain is {scope}",
                journal.scope_id()
            )));
        }
        self.state.lock().reminder_journal = Some(journal);
        Ok(())
    }

    /// The reminder this committed window still needs, if any.
    ///
    /// Preparation is non-consuming on purpose: prompt assembly can be
    /// refused, aborted or restarted after the reminder was read, and a
    /// consuming read would lose the only reminder this window gets. The
    /// answer belongs to the successful commit, so
    /// [`Self::acknowledge_rollover_reminder`] is what spends it.
    ///
    /// The reminder is a prompt-suffix fact, not a window: it never prepares,
    /// publishes or commits anything. A registry with no observed pressure or
    /// no advertised threshold reports nothing.
    pub fn pending_rollover_reminder(&self) -> Option<RolloverReminder> {
        let (threshold, pressure) = {
            let state = self.state.lock();
            (state.reminder?, state.pressure.clone()?)
        };
        if pressure.max_context == 0 {
            return None;
        }
        if (pressure.used_tokens as f32) < (pressure.max_context as f32 * threshold.ratio) {
            return None;
        }
        // A ledger read failure must not block the prompt: no reminder is
        // honest, an error is not.
        let window = self.ledger.current().ok()?.id;
        let journal = { self.state.lock().reminder_journal.clone() };
        if let Some(journal) = &journal {
            // The durable row is the authority across restarts. An unreadable
            // answer is not turned into a repeat: a reminder whose own answer
            // cannot be read is not issued again.
            match journal.issued(window) {
                Ok(true) => return None,
                Ok(false) => {}
                Err(_) => return None,
            }
        }
        if self.state.lock().reminder_issued.contains(&window) {
            return None;
        }
        Some(RolloverReminder {
            window,
            used_tokens: pressure.used_tokens,
            max_context: pressure.max_context,
            policy_epoch: threshold.epoch,
        })
    }

    /// Record that the request assembled with `reminder` committed.
    ///
    /// This is the only place the answer is spent, and it runs on the
    /// committed-context lifecycle: a refused, aborted or restarted assembly
    /// never reaches it, so the reminder stays available and a successful
    /// retry still carries it. The answer is fenced on the installation epoch
    /// that advertised the threshold — a strategy retired or replaced between
    /// projection and commit can neither answer its own reminder after the
    /// fact nor spend its successor's threshold, so the successor issues its
    /// own once. Returns whether this call created the durable record; `false`
    /// means the window was already answered or the issuing installation is
    /// gone.
    pub fn acknowledge_rollover_reminder(
        &self,
        reminder: &RolloverReminder,
    ) -> Result<bool, StorageError> {
        let journal = {
            let state = self.state.lock();
            if !state.reminder.is_some_and(|threshold| threshold.epoch == reminder.policy_epoch) {
                return Ok(false);
            }
            state.reminder_journal.clone()
        };
        let first_in_process = self.state.lock().reminder_issued.insert(reminder.window);
        match journal {
            Some(journal) => journal
                .mark_issued(reminder.window, reminder.policy_epoch)
                .map_err(|error| StorageError(error.to_string())),
            None => Ok(first_in_process),
        }
    }

    fn candidates(&self) -> Vec<(Arc<dyn ContextPolicy>, PolicyManifest, u64)> {
        let state = self.state.lock();
        let rank = |id: &str| {
            state.preference.iter().position(|p| p == id).unwrap_or(usize::MAX)
        };
        let mut out: Vec<_> = state
            .installed
            .iter()
            .map(|i| (i.policy.clone(), i.manifest.clone(), i.epoch))
            .collect();
        out.sort_by_key(|(_, m, _)| rank(&m.id));
        out
    }

    fn base(&self, items: &[HistoryItem], epoch: u64) -> Result<WindowBase, StorageError> {
        Ok(WindowBase {
            source_window: self.ledger.current()?.id,
            leaf_seq: items.last().map_or(0, |i| i.at.seq),
            model_generation: self.ledger.model_generation()?,
            policy_epoch: epoch,
        })
    }

    /// Prepare with the preferred usable strategy, falling back on unavailable,
    /// non-shrinking, over-limit or unreadable results. No registry lock is
    /// held while strategy code runs.
    ///
    /// This is the slow path: it reads the whole raw history and may render
    /// frames. It must run off the request path.
    ///
    /// Gain is judged against the window the model sees now, not the raw
    /// history: a window must be at least [`MIN_GAIN_RATIO`] smaller than the
    /// current materialized window. Re-cutting the same raw history into the
    /// same (or a marginally smaller) window costs a full prefill for no
    /// benefit, so it is refused instead of repeated whenever the trigger
    /// still fires.
    pub fn prepare(
        &self,
        history: &dyn HistorySource,
        caps: &ModelCaps,
        used_tokens: u32,
    ) -> Result<(PreparedWindow, Vec<Attempt>), PrepareError> {
        self.prepare_for(history, caps, used_tokens, false)
    }

    /// Prepare with the explicit-rollover signal the installed strategy may
    /// read.
    ///
    /// An explicit request is a boundary the model asked for, so the
    /// automatic [`MIN_GAIN_RATIO`] guard does not apply: a small window has
    /// nothing to drop, and refusing it would answer "next boundary" with
    /// silence. The model's own context limit still applies.
    fn prepare_for(
        &self,
        history: &dyn HistorySource,
        caps: &ModelCaps,
        used_tokens: u32,
        rollover: bool,
    ) -> Result<(PreparedWindow, Vec<Attempt>), PrepareError> {
        let items = raw_items(history)?;
        let items = &items[..];
        let current: u32 =
            self.ledger.current()?.effective_entries(history)?.iter().map(WindowEntry::tokens).sum();
        let ceiling = current - (current as f32 * MIN_GAIN_RATIO) as u32;
        let mut attempts = Vec::new();
        for (policy, manifest, epoch) in self.candidates() {
            if let Availability::Unavailable(reason) = policy.availability(caps) {
                attempts.push(Attempt {
                    policy: manifest.id,
                    outcome: Err(PolicyError::Unavailable(reason)),
                });
                continue;
            }
            let input =
                ContextInput { base: self.base(items, epoch)?, items, caps, used_tokens, rollover };
            let result = policy.prepare(&input).and_then(|window| {
                let limit = if window.derivatives.is_empty() {
                    caps.max_context
                } else {
                    caps.image_request_context().unwrap_or(0)
                };
                if window.cost.window_tokens > limit {
                    Err(PolicyError::OverBudget { window: window.cost.window_tokens, limit })
                } else if !rollover && window.cost.window_tokens > ceiling {
                    Err(PolicyError::NoReduction)
                } else {
                    Ok(window)
                }
            });
            match result {
                Ok(window) => {
                    attempts.push(Attempt { policy: manifest.id, outcome: Ok(()) });
                    return Ok((window, attempts));
                }
                Err(error) => attempts.push(Attempt { policy: manifest.id, outcome: Err(error) }),
            }
        }
        Err(PrepareError::NoUsablePolicy { attempts })
    }

    /// Prepare a window and bind it to the live revisions, so it can be
    /// published and later adopted. This is the entry point of the
    /// preparation worker.
    pub fn prepare_candidate(
        &self,
        history: &dyn HistorySource,
        caps: &ModelCaps,
        used_tokens: u32,
    ) -> Result<PreparedCandidate, PrepareError> {
        // An explicit request is taken by the attempt that turns it into a
        // candidate. A candidate that is later dropped puts it back, so the
        // request survives until a window actually commits; it is answered by
        // the committed window, not by an attempt.
        let request = self.state.lock().rollover.take();
        let rollover = request.is_some();
        let prepared = self.prepare_for(history, caps, used_tokens, rollover);
        let (window, attempts) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(request) = request {
                    let mut state = self.state.lock();
                    if state.rollover.is_none() {
                        state.rollover = Some(request);
                    }
                }
                return Err(error);
            }
        };
        let binding = PreparationBinding {
            scope_id: self.ledger.scope_id(),
            source_window: window.base.source_window,
            leaf_seq: window.base.leaf_seq,
            model_generation: window.base.model_generation,
            policy_epoch: window.base.policy_epoch,
            policy_id: window.policy.id.clone(),
        };
        Ok(PreparedCandidate {
            binding,
            policy: window.policy.clone(),
            window,
            attempts,
            caps: caps.clone(),
            prepared_at_ms: now_ms(),
            rollover,
        })
    }

    /// Publish a prepared window. The binding is checked against the live
    /// state: another branch, a moved head, a changed model generation or a
    /// replaced strategy all refuse the publication instead of letting a
    /// stale window overwrite a newer one.
    pub fn publish(&self, candidate: PreparedCandidate) -> Result<(), PublishError> {
        self.verify_binding(&candidate)?;
        let diagnostic = PreparationDiagnostic {
            policy: candidate.binding.policy_id.clone(),
            outcome: PreparationOutcome::Published,
            detail: format!(
                "bound to scope {} / window {} / leaf {} / generation {}{}",
                candidate.binding.scope_id,
                candidate.binding.source_window,
                candidate.binding.leaf_seq,
                candidate.binding.model_generation,
                if candidate.rollover { " (explicit rollover request)" } else { "" }
            ),
            at_ms: now_ms(),
        };
        let mut state = self.state.lock();
        state.candidate = Some(candidate);
        state.last_preparation = Some(diagnostic);
        Ok(())
    }

    /// The published candidate waiting for adoption.
    pub fn candidate(&self) -> Option<PreparedCandidate> {
        self.state.lock().candidate.clone()
    }

    /// Prepare and publish in one call — the off-S0 entry point.
    ///
    /// The slow work happens here: the whole raw history is read and the new
    /// representation is rendered. The result is bound to the live revisions
    /// and published, so a later request can adopt it without doing any of
    /// that work itself. A candidate that no longer matches the live state
    /// is refused here, not silently carried forward.
    pub fn prepare_and_publish(
        &self,
        history: &dyn HistorySource,
        caps: &ModelCaps,
        used_tokens: u32,
    ) -> Result<PreparationOutcome, PrepareError> {
        let candidate = self.prepare_candidate(history, caps, used_tokens)?;
        self.publish(candidate).map_err(PrepareError::Publish)?;
        Ok(PreparationOutcome::Published)
    }

    /// The visible outcome of the last preparation.
    pub fn last_preparation(&self) -> Option<PreparationDiagnostic> {
        self.state.lock().last_preparation.clone()
    }

    fn verify_binding(&self, candidate: &PreparedCandidate) -> Result<(), PublishError> {
        if candidate.binding.scope_id != self.ledger.scope_id() {
            return Err(PublishError::ScopeMismatch {
                expected: self.ledger.scope_id(),
                prepared: candidate.binding.scope_id,
            });
        }
        let head = self.ledger.current()?;
        if head.id != candidate.binding.source_window {
            return Err(PublishError::StaleSource {
                prepared: candidate.binding.source_window,
                current: head.id,
            });
        }
        let generation = self.ledger.model_generation()?;
        if generation != candidate.binding.model_generation {
            return Err(PublishError::ModelGenerationChanged {
                prepared: candidate.binding.model_generation,
                current: generation,
            });
        }
        let state = self.state.lock();
        let live = state
            .installed
            .iter()
            .any(|i| i.manifest.id == candidate.binding.policy_id && i.epoch == candidate.binding.policy_epoch);
        if !live {
            return Err(PublishError::StalePolicy {
                policy: candidate.binding.policy_id.clone(),
                epoch: candidate.binding.policy_epoch,
            });
        }
        Ok(())
    }

    /// Adopt the published candidate: the request path.
    ///
    /// The binding is verified again — the head, the model generation, the
    /// strategy installation and the scope may all have moved while the
    /// preparation ran. A candidate that no longer matches is dropped, so the
    /// next preparation starts from the current revisions; the request then
    /// keeps the window it already has.
    ///
    /// The capacity check runs under the exact limits the candidate was
    /// prepared for, including the raw events that arrived after the
    /// preparation read the history: a candidate that no longer fits is
    /// refused instead of silently exceeding the model's context.
    pub fn adopt_prepared(
        &self,
        history: &dyn HistorySource,
    ) -> Result<Option<(CommittedWindow, Vec<Attempt>)>, CommitError> {
        let published = { self.state.lock().candidate.clone() };
        let Some(candidate) = published else {
            return Ok(None);
        };
        if let Err(error) = self.verify_binding(&candidate) {
            self.drop_candidate(&candidate, PreparationOutcome::Stale, error.to_string());
            return Ok(None);
        }
        let leaf = history.leaf_seq().map_err(CommitError::Storage)?;
        if leaf.is_none_or(|leaf| candidate.binding.leaf_seq > leaf) {
            let detail = format!(
                "prepared leaf {} is ahead of the history leaf {leaf:?}",
                candidate.binding.leaf_seq
            );
            self.drop_candidate(&candidate, PreparationOutcome::Stale, detail.clone());
            return Err(CommitError::LeafAhead { prepared: candidate.binding.leaf_seq, leaf });
        }
        // Capacity under the exact profile the candidate was prepared for,
        // including the events that arrived after it read the history.
        let total: u32 = candidate.window.entries.iter().map(WindowEntry::tokens).sum::<u32>()
            + tail_tokens(history, candidate.binding.leaf_seq)?;
        let limit = candidate.request_limit();
        if total > limit {
            let detail =
                format!("materialized window of {total} tokens exceeds the {limit}-token limit");
            self.drop_candidate(&candidate, PreparationOutcome::Refused, detail);
            return Ok(None);
        }
        // The registry's commit, not the ledger's: it holds the installation
        // lock across the append, so a strategy disposed or replaced between
        // the binding check above and this line cannot commit a retired
        // epoch's window. The ledger alone checks window/model/leaf identity.
        match self.commit(candidate.window.clone(), history) {
            Ok(window) => {
                // The committed window is what answers an explicit request:
                // the durable row records which window answered it, so a
                // restart cannot cut the same boundary again. A journal that
                // cannot record the answer cannot undo a committed window; the
                // diagnostic says so instead of hiding it.
                let answered = if candidate.rollover {
                    let journal = self.state.lock().rollover_journal.clone();
                    match journal.map(|journal| journal.consume(window.id)) {
                        Some(Ok(_)) | None => String::new(),
                        Some(Err(error)) => {
                            format!("; explicit rollover request was not marked answered: {error}")
                        }
                    }
                } else {
                    String::new()
                };
                let mut state = self.state.lock();
                if candidate.rollover {
                    state.rollover = None;
                }
                state.candidate = None;
                state.last_preparation = Some(PreparationDiagnostic {
                    policy: candidate.binding.policy_id.clone(),
                    outcome: PreparationOutcome::Adopted,
                    detail: format!(
                        "committed window {} at generation {}{answered}",
                        window.id, window.generation
                    ),
                    at_ms: now_ms(),
                });
                Ok(Some((window, candidate.attempts)))
            }
            Err(CommitError::Storage(error)) => Err(CommitError::Storage(error)),
            Err(other) => {
                let detail = other.to_string();
                self.drop_candidate(&candidate, PreparationOutcome::Stale, detail);
                Ok(None)
            }
        }
    }

    fn drop_candidate(
        &self,
        candidate: &PreparedCandidate,
        outcome: PreparationOutcome,
        detail: String,
    ) {
        // A dropped candidate answered nothing. An explicit request goes back
        // to pending, so the next safe boundary still rolls over; the refusal
        // itself stays visible in the diagnostic.
        let pending = if candidate.rollover {
            let journal = self.state.lock().rollover_journal.clone();
            journal.and_then(|journal| journal.pending().ok().flatten())
        } else {
            None
        };
        let mut state = self.state.lock();
        if let Some(request) = pending {
            if state.rollover.is_none() {
                state.rollover = Some(request);
            }
        }
        if state
            .candidate
            .as_ref()
            .is_some_and(|published| published.binding == candidate.binding)
        {
            state.candidate = None;
        }
        state.last_preparation = Some(PreparationDiagnostic {
            policy: candidate.binding.policy_id.clone(),
            outcome,
            detail,
            at_ms: now_ms(),
        });
    }

    /// Commit only if the preparing installation is still current, then let
    /// the ledger verify source window, model generation and leaf.
    pub fn commit(
        &self,
        prepared: PreparedWindow,
        history: &dyn HistorySource,
    ) -> Result<CommittedWindow, CommitError> {
        // Held across the ledger commit so uninstall/replace cannot interleave.
        let state = self.state.lock();
        let live = state
            .installed
            .iter()
            .any(|i| i.manifest.id == prepared.policy.id && i.epoch == prepared.base.policy_epoch);
        if !live {
            return Err(CommitError::StalePolicy {
                policy: prepared.policy.id,
                epoch: prepared.base.policy_epoch,
            });
        }
        self.ledger.commit(prepared, history)
    }
}

/// Install `policy` into the registry for the lifetime of this Fiber
/// generation. Restart/update gives a new epoch; dispose uninstalls it.
pub fn install_on(ctx: &Context, policy: Arc<dyn ContextPolicy>) -> Result<u64, std::io::Error> {
    let registry = ctx.try_service::<PolicyRegistry>().map_err(std::io::Error::other)?;
    let id = policy.manifest().id;
    let epoch = registry.install(policy).map_err(std::io::Error::other)?;
    ctx.effect_sync(move || {
        registry.uninstall(&id, epoch);
    })
    .map_err(std::io::Error::other)?;
    Ok(epoch)
}

/// Publishes a caller-owned registry. The registry (and its window ledger)
/// outlives this plugin's restarts; strategies require it by name.
pub struct ContextPolicyHost;

impl Plugin for ContextPolicyHost {
    type Config = Arc<PolicyRegistry>;
    type Input = Arc<PolicyRegistry>;
    type PrepareError = std::convert::Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("context-policy-host")
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        // Generation-owned: Cordis withdraws the publication on dispose.
        let _publication = ctx.provide(input.clone()).map_err(std::io::Error::other)?;
        Ok(())
    }
}
