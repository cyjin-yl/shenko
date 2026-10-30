//! Native finite-child registry and dispatcher on the pinned Cordis lifecycle.
//! Model output can request a definition, but only this typed Service creates
//! CallContext/authority. Natural-language goals are not executable definitions.
//!
//! Dispatch is claim-driven: `run_once` only reads durable candidates, performs
//! the synchronous CAS claim and spawns a bounded managed execution. Slow or
//! unknown handlers no longer hold the dispatcher pass hostage, so a finished
//! short child can project its T2/result while a long child is still running.
//! Candidates are visited in least-recent-admission order, so a wave of
//! repeatedly unresolved reconciliations cannot recapture every worker slot
//! and starve a later ready child. Worker lifetime is bound to the Cordis
//! teardown registered by `NativeChildPlugin`: shutdown closes admission and
//! aborts/joins workers before it returns, independent of any retained
//! `Arc<NativeChildService>`.
//! Spawning never escapes the store contract: the claim fences task identity
//! before any handler code runs, a rejected/late completion only leaves a
//! durable park, and restart recovery is unchanged because a `Running` task
//! whose owner died is reconciled from its own memos, never from task handles.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use cordis_core::{Context, InjectSpec, Plugin, Service};
use parking_lot::RwLock;
use shenko_store::{
    Store, TaskCancellation,
    child::{
        COGNITION_IMPORTED, NATIVE_CHILD, NATIVE_INPUT, NATIVE_OPERATION_ID, NATIVE_RECOVERY,
        NativeTaskClaim, memo_flag, memo_uuid, park_reason,
    },
};
use shenko_types::{
    agent::CallContext,
    ids::{EntityId, RecordRevision, RuntimeGeneration},
    object::Digest,
    operation::{AuthorityRef, OperationRecord, OperationState},
    subject::{DelegationIntent, StepDecision},
    task::*,
};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    sync::Arc,
};

/// A slow child may keep a bounded number of managed executions alive; the
/// dispatcher itself is never one of them.
const MAX_INFLIGHT_EXECUTIONS: usize = 8;

enum WorkerJob {
    /// Fresh T1 claim: run the handler, then settle the resolved operation.
    Execute {
        claim: NativeTaskClaim,
        invocation: NativeTaskInvocation,
    },
    /// Recovery settlement: query the original provider handle/receipt only,
    /// then claim the same operation settlement-only. No replay happens here.
    Settle {
        task: TaskRecord,
        operation_id: EntityId,
        implementation: ImplementationRef,
    },
}

struct Worker {
    task_id: EntityId,
    handle: tokio::task::JoinHandle<()>,
}

/// In-memory worker bookkeeping: live handles and shutdown state. Durable
/// claim/park records remain the only dispatch truth — nothing here survives
/// restart or decides recovery.
#[derive(Default)]
struct Executions {
    workers: HashMap<EntityId, Worker>,
    /// Once closed no new claim or worker may be created; Cordis teardown sets
    /// this before joining in-flight workers so `stop()` is the barrier.
    closed: bool,
    /// Monotonic counter bumped by every `reopen` (i.e. every `apply`). A
    /// teardown effect captured at epoch N is stale once a newer apply ran —
    /// it must not close a freshly reactivated service.
    apply_epoch: u64,
}

/// One candidate's dispatch outcome within a pass.
enum Visit {
    /// Durable state advanced (claim, park, settlement or spawn).
    Progressed,
    /// Checked but nothing to do (invisible, not-yet-eligible, fenced...).
    Skipped,
    /// Worker budget exhausted — the scan must pause HERE so the cursor
    /// stays at the last visited candidate and this one is retried first
    /// next pass. Never skip past it.
    NoCapacity,
}

/// Why a worker admission check may refuse a candidate.
enum Admission {
    /// A slot is free and no worker owns this task.
    Open,
    /// This task already has a live managed execution — a Skip, not a global
    /// capacity failure: later candidates must still be visited.
    AlreadyManaged,
    /// Every worker slot is occupied; the scan must pause at this candidate.
    Full,
    /// Teardown closed admission; treated as capacity-blocked.
    Closed,
}

#[derive(Debug, Clone)]
pub struct NativeTaskDescriptor {
    pub kind: String,
    pub definition_version: u32,
    pub implementation_ref: ImplementationRef,
    pub role_revision: String,
    pub authority_refs: Vec<String>,
    /// Model-facing usage shown only while this definition is installed.
    pub prompt_section: Option<String>,
}

pub struct NativeTaskInvocation {
    pub context: CallContext,
    pub task_invocation: shenko_store::TaskInvocation,
    pub task: TaskRecord,
    pub operation_id: EntityId,
    pub authority: AuthorityRef,
    pub input: serde_json::Value,
}

#[async_trait]
pub trait NativeTaskHandler: Send + Sync + 'static {
    fn descriptor(&self) -> NativeTaskDescriptor;

    /// T1 and external effects belong to the handler. The returned operation is
    /// evidence, not authority to mark the task complete by itself.
    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord>;

    /// Recovery may query the original provider handle/receipt. It receives no
    /// new CallContext and must never arm another attempt.
    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId)
    -> Result<OperationRecord>;
}

#[derive(Clone)]
struct RegisteredHandler {
    epoch: u64,
    descriptor: NativeTaskDescriptor,
    handler: Arc<dyn NativeTaskHandler>,
}

#[derive(Default)]
struct RegistryState {
    revision: u64,
    handlers: BTreeMap<(String, u32), RegisteredHandler>,
}

/// The one native runtime registry. It is published/withdrawn by Cordis, not by
/// a model-facing tool schema or a second in-memory service locator.
pub struct NativeChildService {
    store: Arc<Store>,
    generation: RuntimeGeneration,
    instance_id: EntityId,
    registry: RwLock<RegistryState>,
    run_lock: tokio::sync::Mutex<()>,
    /// Handles of spawned managed executions, keyed by task identity. A task
    /// with a live worker is already `Running` in the store; the handle only
    /// bounds local concurrency and drives settlement, it is never the source
    /// of truth for recovery. The same lock guards the shutdown flag.
    executions: parking_lot::Mutex<Executions>,
    /// In-memory dispatch scan over (created_at, task_id). `upper` bounds the
    /// round so arriving later work cannot postpone wraparound; neither value
    /// is a completion watermark and neither survives a restart. The cursor
    /// only advances over candidates a pass actually visited, so a full
    /// worker budget pauses the scan instead of skipping unseen work.
    scan: std::sync::Mutex<ScanState>,
}

#[derive(Default)]
struct ScanState {
    cursor: Option<(String, EntityId)>,
    upper: Option<(String, EntityId)>,
}

impl Service for NativeChildService {
    const NAME: &'static str = "shenko.child.native/v1";
}

impl NativeChildService {
    pub fn new(store: Arc<Store>, generation: RuntimeGeneration) -> Self {
        Self {
            store,
            generation,
            instance_id: EntityId::new(),
            registry: RwLock::new(RegistryState::default()),
            run_lock: tokio::sync::Mutex::new(()),
            executions: parking_lot::Mutex::new(Executions::default()),
            scan: std::sync::Mutex::new(ScanState::default()),
        }
    }

    pub fn recover(&self) -> Result<usize> {
        self.store.recover_native_tasks()
    }

    pub fn cancel(
        &self,
        task_id: EntityId,
        reason: &str,
        scope: AbortScope,
    ) -> Result<TaskCancellation> {
        // This decision is created by the current native service, not by an old
        // owner callback or a model-supplied authority field.
        self.store.request_native_cancellation(
            task_id,
            EntityId::new(),
            EntityId::new(),
            reason,
            scope,
        )
    }

    /// Register a handler for the current Cordis generation. `pub(crate)` so
    /// the plugin entry and in-crate dispatch tests share the one path; the
    /// registry still rejects generic or duplicate definitions.
    pub(crate) fn install(
        self: &Arc<Self>,
        handler: Arc<dyn NativeTaskHandler>,
    ) -> Result<((String, u32), u64)> {
        let descriptor = handler.descriptor();
        ensure!(
            descriptor.kind != "delegation"
                && !descriptor.kind.is_empty()
                && descriptor.definition_version > 0,
            "generic_or_invalid_native_definition"
        );
        ensure!(
            descriptor
                .prompt_section
                .as_ref()
                .is_none_or(|section| { !section.trim().is_empty() && section.len() <= 8192 }),
            "invalid_native_prompt_section"
        );
        let key = (descriptor.kind.clone(), descriptor.definition_version);
        let mut registry = self.registry.write();
        ensure!(
            !registry.handlers.contains_key(&key),
            "native_definition_already_registered"
        );
        registry.revision += 1;
        let epoch = registry.revision;
        let mut descriptor = descriptor;
        descriptor.implementation_ref.registry_revision = epoch;
        registry.handlers.insert(
            key.clone(),
            RegisteredHandler {
                epoch,
                descriptor,
                handler,
            },
        );
        Ok((key, epoch))
    }

    fn uninstall(&self, key: &(String, u32), epoch: u64) {
        let mut registry = self.registry.write();
        if registry
            .handlers
            .get(key)
            .is_some_and(|registered| registered.epoch == epoch)
        {
            registry.handlers.remove(key);
            registry.revision += 1;
        }
    }

    fn handler(&self, kind: &str, version: u32) -> Option<RegisteredHandler> {
        self.registry
            .read()
            .handlers
            .get(&(kind.to_string(), version))
            .cloned()
    }

    /// Admission can report an unavailable implementation before creating new
    /// work. Dispatch still rechecks the Cordis generation when claiming it.
    pub fn has_handler(&self, kind: &str, version: u32) -> bool {
        self.registry.read().handlers.keys()
            .any(|(registered_kind, registered_version)| registered_kind == kind && *registered_version == version)
    }

    /// One bounded pass over durable candidates. Duplicate wakeups only reread
    /// this state; CAS determines whether a handler/attempt may be created.
    /// Finished managed executions are reclaimed first, so a completed short
    /// child projects its result even while another child keeps running.
    /// Handler waits themselves happen inside spawned executions, never under
    /// this pass's lock. The pass visits least-recently-admitted work first so
    /// jobs that keep finishing unresolved cannot re-fill every slot before a
    /// ready sibling is even seen.
    pub async fn run_once(self: &Arc<Self>) -> Result<usize> {
        let _guard = self.run_lock.lock().await;
        if self.executions.lock().closed {
            return Ok(0);
        }
        self.reclaim_finished().await;
        const PAGE: usize = 32;
        let (after, upper, fresh_round) = {
            let state = self.scan.lock()
                .map_err(|_| anyhow::anyhow!("native dispatch scan state poisoned"))?;
            (state.cursor.clone(), state.upper.clone(), state.cursor.is_none())
        };
        let upper = if fresh_round { self.store.native_task_scan_upper()? } else { upper };
        self.scan_once(after, upper, fresh_round, PAGE)
    }

    /// Fetch one page after `after`, visit candidates under the worker
    /// budget, then advance the scan state. `pub(crate)` so in-crate tests
    /// can replay the empty-upper-read → insertion race window
    /// deterministically: passing (fresh_round=true, upper=None) with real
    /// committed candidates is exactly the state the window leaves behind,
    /// and the cursor/upper bookkeeping below is the code under test.
    pub(crate) fn scan_once(
        self: &Arc<Self>,
        after: Option<(String, EntityId)>,
        upper: Option<(String, EntityId)>,
        fresh_round: bool,
        page: usize,
    ) -> Result<usize> {
        let mut upper = upper;
        let candidates = self.store.native_task_candidates_after(
            after.as_ref().map(|(created, id)| (created.as_str(), *id)),
            page,
        )?;
        // Empty-upper race closure: the boundary read observed nothing, but a
        // producer inserted candidates before this fetch. Freeze the round
        // boundary at THIS page's last key before capacity-limited visitation,
        // so a partially visited page keeps a finite upper bound and the
        // round can still wrap; a fully visited or empty page behaves exactly
        // as an upper=None round did.
        if fresh_round && upper.is_none() {
            upper = candidates
                .last()
                .map(|(created, task)| (created.clone(), task.task_id));
        }
        // Advance the scan only over candidates this pass actually visited.
        // Worker capacity is finite, so a fetched-but-unvisited candidate
        // must keep the cursor at the last visited key — the next pass
        // reclaims finished workers and resumes at the first unvisited
        // candidate, so nothing between pages is ever skipped.
        let mut progressed = 0;
        let mut last_visited: Option<(String, EntityId)> = None;
        let mut exhausted = false;
        for (created, task) in candidates.iter() {
            match self.process(task)? {
                Visit::NoCapacity => { exhausted = true; break; }
                Visit::Progressed => progressed += 1,
                Visit::Skipped => {}
            }
            last_visited = Some((created.clone(), task.task_id));
        }
        let visited_last = candidates.last().map(|(c, t)| (c.clone(), t.task_id));
        // A fully visited page completes the round when it was short or it
        // reached the round's upper bound. A partially visited page only
        // completes when the visit already reached the upper bound.
        let complete = if !exhausted {
            (fresh_round && upper.is_none())
                || candidates.len() < page
                || visited_last.as_ref().is_some_and(|last| upper.as_ref().is_some_and(|upper| last >= upper))
        } else {
            last_visited.as_ref().is_some_and(|last| upper.as_ref().is_some_and(|upper| last >= upper))
        };
        {
            let mut state = self.scan.lock()
                .map_err(|_| anyhow::anyhow!("native dispatch scan state poisoned"))?;
            if complete {
                state.cursor = None;
                state.upper = None;
            } else {
                state.cursor = last_visited.or(state.cursor.take());
                state.upper = upper;
            }
        }
        Ok(progressed)
    }

    /// Reclaim every already-finished managed execution. A panicked worker
    /// left the task `Running` without durable settlement; it is parked as
    /// reconciling so the normal recovery/reconcile path—not a blind replay—
    /// decides the original operation's fate.
    async fn reclaim_finished(&self) {
        let finished: Vec<Worker> = {
            let mut executions = self.executions.lock();
            let ids: Vec<EntityId> = executions
                .workers
                .iter()
                .filter(|(_, worker)| worker.handle.is_finished())
                .map(|(task_id, _)| *task_id)
                .collect();
            ids.into_iter()
                .filter_map(|task_id| executions.workers.remove(&task_id))
                .collect()
        };
        for worker in finished {
            match worker.handle.await {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(task_id=%worker.task_id, %error, "managed native execution lost; parking for reconciliation");
                    if let Ok(task) = self.store.external_task(worker.task_id) {
                        park_managed(
                            &self.store,
                            &task,
                            "reconciling",
                            "managed execution ended without a durable settlement; reconcile original evidence",
                            true,
                        );
                    }
                }
            }
        }
    }

    /// Whether this task may own a worker right now. `AlreadyManaged` is a
    /// per-task skip (its execution is in flight); `Full`/`Closed` are global
    /// conditions that must pause the whole scan.
    fn admission(&self, task_id: EntityId) -> Admission {
        let executions = self.executions.lock();
        if executions.workers.contains_key(&task_id) {
            return Admission::AlreadyManaged;
        }
        if executions.closed {
            return Admission::Closed;
        }
        if executions.workers.len() >= MAX_INFLIGHT_EXECUTIONS {
            return Admission::Full;
        }
        Admission::Open
    }

    /// Atomically install a spawned worker. Returns false when teardown closed
    /// admission or the budget filled between the caller's pre-check and this
    /// point; the caller must then leave the already-durable claim to durable
    /// recovery, never detach a worker.
    fn spawn_execution(
        &self,
        task_id: EntityId,
        registered: RegisteredHandler,
        job: WorkerJob,
    ) -> bool {
        let mut executions = self.executions.lock();
        if executions.closed
            || executions.workers.len() >= MAX_INFLIGHT_EXECUTIONS
            || executions.workers.contains_key(&task_id)
        {
            return false;
        }
        let store = self.store.clone();
        let handle = tokio::spawn(async move {
            drive_execution(store, registered, job).await;
        });
        executions.workers.insert(task_id, Worker { task_id, handle });
        true
    }

    /// Reactivate a service whose Cordis Fiber restarted on the SAME sealed
    /// input. `fiber.restart()` calls `apply` again without re-running
    /// `prepare`, so the same `Arc<NativeChildService>` comes back — still
    /// `closed` from the previous teardown. Returns the apply epoch that
    /// this activation's teardown must carry; a stale epoch belongs to an
    /// old generation and is ignored.
    ///
    /// Ordering: `run_lock` serializes this behind any in-flight
    /// `close_and_join`, so reactivation only happens after the previous
    /// generation's workers were fully joined. Aborted claims are first
    /// recovered into durable query-only reconciliation
    /// (`recover_native_tasks` flips Running → NATIVE_RECOVERY + Ready),
    /// THEN admission reopens — a task aborted mid-execution can never be
    /// re-armed as a duplicate unknown effect.
    pub async fn reopen(&self) -> Result<u64> {
        let _guard = self.run_lock.lock().await;
        // Defensive: the barrier should have drained these, but a worker can
        // never be re-admitted without a clean book regardless.
        let leftover: Vec<Worker> = {
            let mut executions = self.executions.lock();
            executions.apply_epoch += 1;
            executions.workers.drain().map(|(_, worker)| worker).collect()
        };
        for worker in &leftover {
            worker.handle.abort();
        }
        for worker in leftover {
            let _ = worker.handle.await;
        }
        self.store.recover_native_tasks()?;
        let mut executions = self.executions.lock();
        executions.closed = false;
        Ok(executions.apply_epoch)
    }

    /// Cordis teardown: close admission, abort every managed execution and
    /// join them. `run_lock` is taken first so a pass already in flight
    /// finishes its synchronous CAS work before admission closes, and no pass
    /// may start once teardown owns it. `epoch` is the apply generation this
    /// effect was registered under — if a same-Fiber restart already bumped
    /// it, this stale teardown no-ops instead of killing the new generation.
    /// Workers stop at their next yield; anything they armed stays durable
    /// (Running/claim or explicit park) and the next runtime generation
    /// reconciles it — never replays it. Runs under `stop()`, so no worker
    /// effect or T2 commit can land after shutdown reports completion; a
    /// retained `Arc` cannot keep workers alive past it, and `Drop` remains
    /// only the fallback for a service that never ran inside Cordis.
    pub async fn close_and_join(&self, epoch: u64) {
        let _guard = self.run_lock.lock().await;
        let workers: Vec<Worker> = {
            let mut executions = self.executions.lock();
            if executions.apply_epoch != epoch {
                // A newer apply already reactivated this service; aborting
                // now would kill the CURRENT generation's workers.
                return;
            }
            executions.closed = true;
            executions.workers.drain().map(|(_, worker)| worker).collect()
        };
        for worker in &workers {
            worker.handle.abort();
        }
        for worker in workers {
            // An aborted worker reports cancelled and leaves its task Running:
            // durable truth, reconciled on the next generation. A panicked
            // worker gets the same explicit reconciling park as a reclaimed
            // one so no execution disappears without a durable record.
            match worker.handle.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    tracing::warn!(task_id=%worker.task_id, %error, "managed native execution lost during teardown; parking for reconciliation");
                    if let Ok(task) = self.store.external_task(worker.task_id) {
                        park_managed(
                            &self.store,
                            &task,
                            "reconciling",
                            "managed execution ended without a durable settlement; reconcile original evidence",
                            true,
                        );
                    }
                }
            }
        }
    }

    /// Deterministic guidance from currently installed handlers, ordered by
    /// frozen kind/version. No registry entry means no advertised capability.
    pub fn installed_prompt_sections(&self) -> Vec<String> {
        self.registry
            .read()
            .handlers
            .values()
            .filter_map(|registered| registered.descriptor.prompt_section.clone())
            .collect()
    }

    /// The registered `(kind, definition_version)` pairs, ordered. This is the
    /// same registry the prompt sections come from, surfaced so self-knowledge
    /// can report what this instance can actually execute — SPEC §38.
    pub fn installed_definitions(&self) -> Vec<(String, u32)> {
        self.registry
            .read()
            .handlers
            .keys()
            .cloned()
            .collect()
    }

    /// Crate-visible view of the in-memory scan position for tests and
    /// diagnostics. It is scheduling state only — never a completion
    /// watermark and never consulted for durable truth.
    pub(crate) fn scan_snapshot(
        &self,
    ) -> (Option<(String, EntityId)>, Option<(String, EntityId)>) {
        match self.scan.lock() {
            Ok(state) => (state.cursor.clone(), state.upper.clone()),
            Err(_) => (None, None),
        }
    }

    /// Advance one durable candidate by at most one synchronous CAS step. Any
    /// handler wait (fresh `execute` or recovery `reconcile`) is delegated to a
    /// spawned managed execution; this pass itself never awaits a handler.
    /// `NoCapacity` means the worker budget ran out BEFORE this candidate was
    /// touched — the caller must stop the scan without advancing the cursor
    /// past it, so it is retried first on the next pass.
    fn process(&self, task: &TaskRecord) -> Result<Visit> {
        if task.memos.contains_key(COGNITION_IMPORTED) {
            // Do not even rewrite the import into an ordinary retryable park.
            // Only an explicit local continuation may remove this marker.
            return Ok(Visit::Skipped);
        }
        if matches!(task.state, TaskState::Completing { .. }) {
            return Ok(if self.store.finish_native_completing(task.task_id)?.is_some() {
                Visit::Progressed
            } else {
                Visit::Skipped
            });
        }
        if task.is_terminal() || task.is_running() {
            return Ok(Visit::Skipped);
        }
        if !task.memos.contains_key(NATIVE_INPUT) {
            if park_reason(task) != Some("definition_unavailable") {
                self.store.park_native_task(
                    task.task_id,
                    task.record_revision,
                    "definition_unavailable",
                    "natural-language delegation has no deterministic native definition",
                    false,
                )?;
                return Ok(Visit::Progressed);
            }
            return Ok(Visit::Skipped);
        }
        if task.abort_request.is_some() {
            // Cancellation already fenced new claims. EvidenceIngress remains
            // independent; no current result/outbox is derived from late facts.
            return Ok(Visit::Skipped);
        }
        if let DispatchEligibility::Parked {
            until_utc: Some(until),
            ..
        } = &task.dispatch_eligibility
        {
            if until > &chrono::Utc::now() {
                return Ok(Visit::Skipped);
            }
        }
        let registered = match self.handler(&task.kind, task.definition_version) {
            Some(registered) => registered,
            None => {
                self.store.park_native_task(
                    task.task_id,
                    task.record_revision,
                    "handler_unavailable",
                    "no Cordis handler for the frozen kind/version",
                    true,
                )?;
                return Ok(Visit::Progressed);
            }
        };
        if memo_flag(task, NATIVE_CHILD)
            && (task.implementation_ref.plugin_id
                != registered.descriptor.implementation_ref.plugin_id
                || task.implementation_ref.artifact_digest
                    != registered.descriptor.implementation_ref.artifact_digest)
        {
            self.park_current(
                task.task_id,
                "implementation_unavailable",
                "the frozen native implementation is not registered; install the original artifact",
                false,
            )?;
            return Ok(Visit::Progressed);
        }
        let operation_id = memo_uuid(task, NATIVE_OPERATION_ID);
        let existing = match operation_id {
            Some(id) => self.store.get_operation(id)?,
            None => None,
        };
        let recovery = memo_flag(task, NATIVE_RECOVERY) || existing.is_some();
        if recovery && existing.is_some() {
            let operation_id = operation_id.expect("checked above");
            let operation = existing.expect("checked above");
            if operation.state != OperationState::Resolved {
                // The original provider query runs in a managed execution so a
                // slow or unreachable handle cannot hold the dispatcher pass.
                match self.admission(task.task_id) {
                    // An in-flight reconciliation already owns this task; the
                    // candidate is visited and the scan must continue.
                    Admission::AlreadyManaged => return Ok(Visit::Skipped),
                    Admission::Full | Admission::Closed => return Ok(Visit::NoCapacity),
                    Admission::Open => {}
                }
                let implementation = registered.descriptor.implementation_ref.clone();
                if !self.spawn_execution(
                    task.task_id,
                    registered,
                    WorkerJob::Settle {
                        task: task.clone(),
                        operation_id,
                        implementation,
                    },
                ) {
                    return Ok(Visit::NoCapacity);
                }
                return Ok(Visit::Progressed);
            }
            let Some(claim) = self.store.claim_native_task(
                task.task_id,
                registered.descriptor.implementation_ref.clone(),
                true,
            )?
            else {
                return Ok(Visit::Skipped);
            };
            return Ok(if self.store.complete_native_task(&claim, &operation)?.is_some() {
                Visit::Progressed
            } else {
                Visit::Skipped
            });
        }
        if matches!(park_reason(task), Some(reason) if !matches!(reason,
            "handler_unavailable" | "reconciling") && !recovery)
        {
            return Ok(Visit::Skipped);
        }
        // The claim must never be created without a spawned owner to settle
        // it; capacity is checked before the CAS transitions Pending→Running.
        match self.admission(task.task_id) {
            Admission::AlreadyManaged => return Ok(Visit::Skipped),
            Admission::Full | Admission::Closed => return Ok(Visit::NoCapacity),
            Admission::Open => {}
        }
        let Some(claim) = self.store.claim_native_task(
            task.task_id,
            registered.descriptor.implementation_ref.clone(),
            false,
        )?
        else {
            return Ok(Visit::Skipped);
        };
        if claim.software_generation != self.generation {
            self.store.park_native_task(
                task.task_id,
                claim.task.record_revision,
                "generation_unavailable",
                "registered runtime generation does not match the sealed task head",
                false,
            )?;
            return Ok(Visit::Progressed);
        }
        let input = match self.native_input(&claim.task) {
            Ok(input) => input,
            Err(error) => {
                self.store.park_native_task(
                    task.task_id,
                    claim.task.record_revision,
                    "input_unavailable",
                    &format!("frozen native input cannot be resolved: {error}"),
                    false,
                )?;
                return Ok(Visit::Progressed);
            }
        };
        let context = CallContext {
            caller_instance_id: self.instance_id,
            role_revision: registered.descriptor.role_revision.clone(),
            task_id: Some(task.task_id),
            expected_task_revision: Some(claim.invocation.task_revision.0),
            invocation_id: Some(claim.invocation.invocation_id),
            controller_epoch: claim.invocation.epoch,
            software_generation: claim.software_generation.clone(),
            authority_refs: registered.descriptor.authority_refs.clone(),
            trace_id: EntityId::new(),
        };
        let authority = AuthorityRef {
            delegation_id: task.task_id,
            delegation_revision: RecordRevision(claim.invocation.task_revision.0),
            decision_ref: claim
                .task
                .created_by_step_id
                .unwrap_or(claim.invocation.invocation_id),
        };
        let invocation = NativeTaskInvocation {
            context,
            task_invocation: claim.invocation.clone(),
            task: claim.task.clone(),
            operation_id: claim.operation_id,
            authority,
            input,
        };
        let task_id = claim.task.task_id;
        if !self.spawn_execution(
            task_id,
            registered,
            WorkerJob::Execute { claim, invocation },
        ) {
            // Teardown closed admission between the pre-check and the spawn,
            // or the budget filled. The claim is already durable; park it as
            // reconciling so the armed operation is recovered from evidence,
            // never replayed or left unowned without a durable record.
            self.park_current(
                task_id,
                "reconciling",
                "durable claim exists but admission closed before a managed execution owned it; reconcile on recovery",
                true,
            )?;
            return Ok(Visit::Progressed);
        }
        Ok(Visit::Progressed)
    }

    fn park_current(
        &self,
        task_id: EntityId,
        reason: &str,
        detail: &str,
        retry: bool,
    ) -> Result<()> {
        let current = self.store.external_task(task_id)?;
        park_or_report(&self.store, task_id, current.record_revision, reason, detail, retry)
    }

    fn native_input(&self, task: &TaskRecord) -> Result<serde_json::Value> {
        let bytes = self.store.cas.read_object(&task.input_ref.digest)?;
        ensure!(
            bytes.len() as u64 == task.input_ref.bytes
                && Digest::blake3(&bytes) == task.input_ref.digest,
            "native_input_object_mismatch"
        );
        match task
            .memos
            .get(NATIVE_INPUT)
            .and_then(|value| value.as_str())
        {
            Some("subject_decision") => {
                let decision: StepDecision = serde_json::from_slice(&bytes)?;
                // Existing finite children can still hold an immutable schema-4
                // S1 decision. Waits use schema 6 and model children schema 7;
                // all bind the same native
                // delegation field without rewriting the historical CAS object.
                ensure!(matches!(decision.schema_version, 4 | 6 | 7), "unsupported_decision_schema");
                let intent = decision
                    .delegated_task_intents
                    .iter()
                    .find(|intent| intent.intent_id == task.task_id)
                    .context("native_delegation_missing_from_frozen_decision")?;
                definition_input(intent, task)
            }
            Some("object") => Ok(serde_json::from_slice(&bytes)?),
            _ => anyhow::bail!("unknown_native_input_encoding"),
        }
    }
}

/// Park from inside a managed execution where errors cannot propagate. The
/// park CAS is retried once against the current record revision; if even that
/// fails the durable state is untouched and normal recovery decides.
/// A task that committed an abort_request keeps its `cancel_requested`
/// disposition: a late worker outcome never relabels a cancelled task.
fn park_managed(store: &Store, task: &TaskRecord, reason: &str, detail: &str, retry: bool) {
    if task.abort_request.is_some() {
        return;
    }
    if let Err(error) = park_or_report(store, task.task_id, task.record_revision, reason, detail, retry) {
        tracing::warn!(task_id=%task.task_id, %error, reason, "managed park rejected; recovery decides");
        if let Ok(current) = store.external_task(task.task_id) {
            if current.abort_request.is_some() {
                return;
            }
            if let Err(error) = park_or_report(store, task.task_id, current.record_revision, reason, detail, retry) {
                tracing::warn!(task_id=%task.task_id, %error, reason, "managed park retry rejected; recovery decides");
            }
        }
    }
}

fn park_or_report(
    store: &Store,
    task_id: EntityId,
    expected_revision: RecordRevision,
    reason: &str,
    detail: &str,
    retry: bool,
) -> Result<()> {
    store.park_native_task(task_id, expected_revision, reason, detail, retry)?;
    Ok(())
}

/// One managed execution lifetime: exactly one durable claim or settlement,
/// one handler wait, then a single settlement attempt. Every outcome that is
/// not a committed T2 leaves an explicit durable park; a lost worker is
/// reclaimed by `reclaim_finished` and a lost process by startup recovery.
async fn drive_execution(
    store: Arc<Store>,
    registered: RegisteredHandler,
    job: WorkerJob,
) {
    match job {
        WorkerJob::Execute { claim, invocation } => {
            let task_id = invocation.task.task_id;
            let operation = match registered.handler.execute(invocation).await {
                Ok(operation) => operation,
                Err(error) => {
                    let current = match store.external_task(task_id) {
                        Ok(task) => task,
                        Err(read_error) => {
                            tracing::warn!(task_id=%task_id, %read_error, "task read failed; recovery decides");
                            return;
                        }
                    };
                    let has_operation =
                        store
                            .get_operation(claim.operation_id)
                            .is_ok_and(|op| op.is_some());
                    park_managed(
                        &store,
                        &current,
                        if has_operation {
                            "reconciling"
                        } else {
                            "handler_failed"
                        },
                        &format!("native handler did not produce a settled result: {error}"),
                        has_operation,
                    );
                    return;
                }
            };
            let Some(task) = fetch_task(&store, task_id) else { return };
            if operation.task_id != task_id || operation.operation_id != claim.operation_id {
                park_managed(
                    &store,
                    &task,
                    "handler_contract_violation",
                    "handler returned an operation with a different task/operation identity",
                    false,
                );
                return;
            }
            if operation.state == OperationState::Resolved {
                settle_claim(&store, &task, &claim, operation);
            } else {
                park_managed(
                    &store,
                    &task,
                    "reconciling",
                    "armed native operation is unresolved; query original evidence before any replay",
                    true,
                );
            }
        }
        WorkerJob::Settle {
            task,
            operation_id,
            implementation,
        } => {
            let operation = match registered.handler.reconcile(&task, operation_id).await {
                Ok(operation) => operation,
                Err(error) => {
                    park_managed(
                        &store,
                        &task,
                        "reconciling",
                        &format!("original operation remains unresolved: {error}"),
                        true,
                    );
                    return;
                }
            };
            if operation.state != OperationState::Resolved {
                park_managed(
                    &store,
                    &task,
                    "reconciling",
                    "original armed operation has no conclusive outcome; no replay",
                    true,
                );
                return;
            }
            let claim = match store.claim_native_task(task.task_id, implementation, true) {
                Ok(Some(claim)) => claim,
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(task_id=%task.task_id, %error, "settlement-only claim failed; recovery decides");
                    if let Some(task) = fetch_task(&store, task.task_id) {
                        park_managed(
                            &store,
                            &task,
                            "reconciling",
                            "settlement-only claim could not be made; reconcile again",
                            true,
                        );
                    }
                    return;
                }
            };
            match store.complete_native_task(&claim, &operation) {
                Ok(Some(_)) | Ok(None) => {}
                Err(error) => {
                    tracing::warn!(task_id=%task.task_id, %error, "native settlement failed; recovery decides");
                    if let Some(task) = fetch_task(&store, task.task_id) {
                        park_managed(
                            &store,
                            &task,
                            "reconciling",
                            "resolved operation could not be committed; reconcile evidence and retry",
                            true,
                        );
                    }
                }
            }
        }
    }
}

/// Settle the stable claim of a fresh execution. `complete_native_task`
/// compares claim identity (task revision, invocation, epoch) against the
/// CURRENT task record, so a cancelled or superseded claim silently keeps the
/// parked state instead of writing a late T2.
fn settle_claim(store: &Store, task: &TaskRecord, claim: &NativeTaskClaim, operation: OperationRecord) {
    match store.complete_native_task(claim, &operation) {
        Ok(Some(_)) | Ok(None) => {}
        Err(error) => {
            tracing::warn!(task_id=%task.task_id, %error, "native settlement failed; recovery decides");
            park_managed(
                store,
                task,
                "reconciling",
                "resolved operation could not be committed; reconcile evidence and retry",
                true,
            );
        }
    }
}

fn fetch_task(store: &Store, task_id: EntityId) -> Option<TaskRecord> {
    match store.external_task(task_id) {
        Ok(task) => Some(task),
        Err(error) => {
            tracing::warn!(task_id=%task_id, %error, "task read failed; recovery decides");
            None
        }
    }
}

impl Drop for NativeChildService {
    /// Dropping the service aborts its managed executions instead of detaching
    /// them. A dropped JoinHandle in tokio keeps running; the task would then
    /// hold a claim no one can settle while the process still lives.
    /// Cordis teardown (`close_and_join`) is the real barrier; this only
    /// covers a service dropped without one.
    fn drop(&mut self) {
        let executions = self.executions.get_mut();
        executions.closed = true;
        for (_, worker) in executions.workers.drain() {
            worker.handle.abort();
        }
    }
}

fn definition_input(intent: &DelegationIntent, task: &TaskRecord) -> Result<serde_json::Value> {
    let definition = intent
        .definition
        .as_ref()
        .context("missing_explicit_native_definition")?;
    ensure!(
        definition.kind == task.kind && definition.definition_version == task.definition_version,
        "native_definition_identity_mismatch"
    );
    Ok(definition.input.clone())
}

pub struct NativeChildPlugin;
impl Plugin for NativeChildPlugin {
    type Config = (Arc<Store>, RuntimeGeneration);
    type Input = Arc<NativeChildService>;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("shenko-native-child-runtime")
    }

    fn prepare(
        &self,
        (store, generation): Self::Config,
    ) -> Result<Self::Input, Self::PrepareError> {
        Ok(Arc::new(NativeChildService::new(store, generation)))
    }

    async fn apply(&self, ctx: Context, service: &Self::Input) -> Result<(), Self::ApplyError> {
        // Same-Fiber restart reuses this sealed service: reactivate only after
        // any in-flight close barrier and after aborted Running claims were
        // recovered into query-only reconciliation. On first publication the
        // drain is empty and recovery only normalizes claims orphaned by a
        // previous process — the same thing callers already do via recover().
        let epoch = service
            .reopen()
            .await
            .map_err(std::io::Error::other)?;
        let _publication = ctx
            .provide(service.clone())
            .map_err(std::io::Error::other)?;
        // Worker lifetime is bound to the runtime, not to the service Arc:
        // shutdown closes admission and aborts/joins every managed execution
        // before `stop()` returns, so a retained service handle can never let
        // a worker commit effects after teardown reported complete.
        let teardown = service.clone();
        ctx.effect(move || async move {
            teardown.close_and_join(epoch).await;
        })
        .map_err(std::io::Error::other)?;
        Ok(())
    }
}

/// Each handler has a real Cordis generation lifetime. Removal prevents new
/// claims while the original operation/reader remains durable for reconciliation.
pub struct NativeTaskHandlerPlugin;
impl Plugin for NativeTaskHandlerPlugin {
    type Config = Arc<dyn NativeTaskHandler>;
    type Input = Arc<dyn NativeTaskHandler>;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("shenko-native-task-handler")
    }

    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(NativeChildService::NAME)
    }

    fn prepare(&self, handler: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(handler)
    }

    async fn apply(&self, ctx: Context, handler: &Self::Input) -> Result<(), Self::ApplyError> {
        let service = ctx
            .try_service::<NativeChildService>()
            .map_err(std::io::Error::other)?;
        let (key, epoch) = service
            .install(handler.clone())
            .map_err(std::io::Error::other)?;
        ctx.effect_sync(move || service.uninstall(&key, epoch))
            .map_err(std::io::Error::other)?;
        Ok(())
    }
}
