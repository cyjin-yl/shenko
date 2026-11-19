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
//! signals/joins actual executions before it returns, independent of any retained
//! `Arc<NativeChildService>`.
//! Spawning never escapes the store contract: the claim fences task identity
//! before any handler code runs, a rejected/late completion only leaves a
//! durable park, and restart recovery is unchanged because a `Running` task
//! whose owner died is reconciled from its own memos, never from task handles.
use anyhow::{Result, ensure};
use async_trait::async_trait;
use cordis_core::{Context, InjectSpec, Plugin, Service};
use parking_lot::RwLock;
use shenko_store::{
    Store, TaskCancellation,
    child::{
        COGNITION_IMPORTED, NATIVE_ASSIGNMENT, NATIVE_CHILD, NATIVE_INPUT, NATIVE_OPERATION_ID, NATIVE_RECOVERY,
        NativeTaskClaim, memo_flag, memo_uuid, park_reason,
    },
};
use shenko_types::{
    agent::CallContext,
    ids::{EntityId, RecordRevision, RuntimeGeneration},
    object::Digest,
    operation::{AuthorityRef, OperationOutcome, OperationRecord, OperationState},
    task::*,
};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
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
    cancellation: tokio_util::sync::CancellationToken,
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

// The handler contract itself is kernel interface
// and it has exactly one owner: `shenko_kernel::task`. This module owns the
// registry that runs the contract, not the contract itself.
use shenko_kernel::task::{
    ChildToolCall, ChildToolOutcome, ChildToolRuntime, NativeTaskDescriptor, NativeTaskHandler,
    NativeTaskDeferred, NativeTaskInvocation,
    execute_native_handler,
};

/// The runtime handle handed to a child. It is the very service that owns
/// the registry, so a child calls the same handlers the subject can call.
pub struct NativeToolRuntime {
    service: Arc<NativeChildService>,
}

#[async_trait]
impl ChildToolRuntime for NativeToolRuntime {
    fn describe_tools(
        &self,
        allowed: &[shenko_types::output::ModelChildTool],
    ) -> Vec<(String, u32, String)> {
        self.service.describe_tools(allowed)
    }

    async fn call_tool(
        &self,
        call: &ChildToolCall,
        parent: &TaskRecord,
        authority: &AuthorityRef,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<ChildToolOutcome> {
        self.service.call_tool_cancellable(call, parent, authority, cancellation).await
    }
}

#[derive(Clone)]
struct RegisteredHandler {
    epoch: u64,
    /// Historical receipt reader only: never discoverable or executable.
    recovery_only: bool,
    descriptor: NativeTaskDescriptor,
    handler: Arc<dyn NativeTaskHandler>,
}

#[derive(Default)]
struct RegistryState {
    revision: u64,
    handlers: BTreeMap<(String, u32, Digest), RegisteredHandler>,
    /// Only the explicitly selected installation accepts new calls. Older
    /// still-installed artifacts remain available for their frozen tasks.
    current: BTreeMap<(String, u32), Digest>,
    retained_artifacts: BTreeSet<Digest>,
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
        let cancelled = self.store.request_native_cancellation(
            task_id,
            EntityId::new(),
            EntityId::new(),
            reason,
            scope,
        )?;
        let executions = self.executions.lock();
        for id in &cancelled.cancelled {
            if let Some(worker) = executions.workers.get(id) { worker.cancellation.cancel(); }
        }
        Ok(cancelled)
    }

    /// Register a handler for the current Cordis generation. `pub(crate)` so
    /// the plugin entry and in-crate dispatch tests share the one path; the
    /// registry still rejects generic or duplicate definitions.
    pub(crate) fn install(
        self: &Arc<Self>,
        handler: Arc<dyn NativeTaskHandler>,
    ) -> Result<((String, u32, Digest), u64)> {
        self.install_handler(handler, false)
    }

    fn install_handler(
        self: &Arc<Self>,
        handler: Arc<dyn NativeTaskHandler>,
        recovery_only: bool,
    ) -> Result<((String, u32, Digest), u64)> {
        let descriptor = handler.descriptor()?;
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
        if !recovery_only {
            let artifact = shenko_kernel::native_artifact::running_native_artifact()?;
            if descriptor.implementation_ref.artifact_digest == artifact.digest()
                && !self.registry.read().retained_artifacts.contains(&artifact.digest())
            {
                let retained = artifact.read(|file| Ok(self.store.cas.seal_reader(
                    file, "application/octet-stream", "native-implementation", "native-executable", 1,
                )?.object))?;
                ensure!(retained.digest == descriptor.implementation_ref.artifact_digest,
                    "running native artifact changed while freezing its recovery object");
                self.registry.write().retained_artifacts.insert(retained.digest);
            }
        }
        let definition = (descriptor.kind.clone(), descriptor.definition_version);
        let key = (definition.0.clone(), definition.1, descriptor.implementation_ref.artifact_digest.clone());
        let mut registry = self.registry.write();
        ensure!(
            !registry.handlers.contains_key(&key),
            "native_definition_already_registered"
        );
        registry.revision += 1;
        let epoch = registry.revision;
        let mut descriptor = descriptor;
        if !recovery_only {
            descriptor.implementation_ref.registry_revision = epoch;
            registry.current.insert(definition, key.2.clone());
        }
        registry.handlers.insert(
            key.clone(),
            RegisteredHandler {
                epoch,
                recovery_only,
                descriptor,
                handler,
            },
        );
        Ok((key, epoch))
    }

    fn uninstall(&self, key: &(String, u32, Digest), epoch: u64) {
        let mut registry = self.registry.write();
        if registry
            .handlers
            .get(key)
            .is_some_and(|registered| registered.epoch == epoch)
        {
            registry.handlers.remove(key);
            let definition = (key.0.clone(), key.1);
            if registry.current.get(&definition) == Some(&key.2) {
                registry.current.remove(&definition);
            }
            registry.revision += 1;
        }
    }

    fn handler(&self, kind: &str, version: u32) -> Option<RegisteredHandler> {
        let registry = self.registry.read();
        let digest = registry.current.get(&(kind.to_owned(), version))?;
        registry.handlers.get(&(kind.to_owned(), version, digest.clone())).cloned()
    }

    fn frozen_handler(&self, task: &TaskRecord) -> Option<RegisteredHandler> {
        if !memo_flag(task, NATIVE_CHILD) { return self.handler(&task.kind, task.definition_version); }
        self.registry.read().handlers.get(&(task.kind.clone(), task.definition_version,
            task.implementation_ref.artifact_digest.clone())).filter(|registered|
                !registered.recovery_only
                && registered.descriptor.implementation_ref.plugin_id == task.implementation_ref.plugin_id).cloned()
    }

    /// A historical reader can only query a genuine durable original attempt.
    /// Its descriptor names the old origin, not this installation's epoch.
    /// Plugin-specific contract digests and frozen payloads remain the reader's
    /// responsibility; the runtime binds the task, operation and attempt here.
    fn recovery_handler(
        &self,
        task: &TaskRecord,
        operation: &OperationRecord,
    ) -> Result<Option<RegisteredHandler>> {
        if !memo_flag(task, NATIVE_CHILD)
            || memo_uuid(task, NATIVE_OPERATION_ID) != Some(operation.operation_id)
            || operation.task_id != task.task_id
            || operation.state == OperationState::Resolved
        {
            return Ok(None);
        }
        let registered = self.registry.read().handlers.get(&(
            task.kind.clone(), task.definition_version,
            task.implementation_ref.artifact_digest.clone(),
        )).filter(|registered| registered.recovery_only).cloned();
        let Some(registered) = registered else { return Ok(None); };
        let descriptor = &registered.descriptor;
        let original = descriptor.implementation();
        if descriptor.implementation_ref.plugin_id != task.implementation_ref.plugin_id
            || operation.contract_ref.plugin_id != task.implementation_ref.plugin_id
            || operation.contract_ref.artifact_digest != task.implementation_ref.artifact_digest
            || operation.contract_ref.receipt_reader_ref != original
        {
            return Ok(None);
        }
        let Some(assignment) = task.memos.get(NATIVE_ASSIGNMENT) else { return Ok(None); };
        let assignment: ExecutionAssignment = serde_json::from_value(assignment.clone())?;
        let attempt = self.store.external_operation_attempt(operation.operation_id)?;
        // Registry revision is local to the original installation. Compare the
        // two persisted identities, never a newly installed reader's revision.
        if assignment.implementation_ref != task.implementation_ref
            || attempt.operation_id != operation.operation_id
            || attempt.implementation_ref != original
            || attempt.invocation_id != assignment.invocation_id
            || attempt.worker_id != assignment.worker_id
            || attempt.node_id != assignment.node_id
            || attempt.epoch != assignment.epoch
        {
            return Ok(None);
        }
        Ok(Some(registered))
    }

    /// Admission can report an unavailable implementation before creating new
    /// work. Dispatch still rechecks the Cordis generation when claiming it.
    pub fn has_handler(&self, kind: &str, version: u32) -> bool {
        self.registry.read().current.keys()
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
        let cancellation = match &job {
            WorkerJob::Execute { invocation, .. } => invocation.cancellation.clone(),
            WorkerJob::Settle { .. } => tokio_util::sync::CancellationToken::new(),
        };
        let worker_cancellation = cancellation.clone();
        let handle = tokio::spawn(async move {
            drive_execution(store, registered, job, worker_cancellation).await;
        });
        executions.workers.insert(task_id, Worker { task_id, cancellation, handle });
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
        let (reactivating, leftover): (bool, Vec<Worker>) = {
            let mut executions = self.executions.lock();
            let reactivating = executions.apply_epoch != 0;
            executions.apply_epoch += 1;
            (reactivating, executions.workers.drain().map(|(_, worker)| worker).collect())
        };
        self.join_workers(leftover).await;
        // First publication only installs the catalog. A running composition
        // root explicitly recovers after it owns the dispatcher; discovery
        // must not need or acquire that durable authority.
        if reactivating {
            self.store.recover_native_tasks()?;
        }
        let mut executions = self.executions.lock();
        executions.closed = false;
        Ok(executions.apply_epoch)
    }

    /// Begin runtime retirement before the hook-quiescence barrier. A model
    /// child can own a hook lease while awaiting a nested guest; signalling
    /// only from the later Cordis teardown would deadlock those two owners.
    /// This is not a completed stop: Cordis still drains and joins the workers.
    pub fn request_stop(&self) {
        let mut executions = self.executions.lock();
        executions.closed = true;
        for worker in executions.workers.values() { worker.cancellation.cancel(); }
    }

    /// Cordis teardown: close admission, request cancellation and
    /// join them. `run_lock` is taken first so a pass already in flight
    /// finishes its synchronous CAS work before admission closes, and no pass
    /// may start once teardown owns it. `epoch` is the apply generation this
    /// effect was registered under — if a same-Fiber restart already bumped
    /// it, this stale teardown no-ops instead of killing the new generation.
    /// Executors stop their local wait or cancel and join their real worker;
    /// anything they armed stays durable
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
        self.join_workers(workers).await;
    }

    /// Signal and join drained executions. Never abort an outer future that
    /// may own a blocking guest. Remote Unknown remains charged and the
    /// broker's original sink can still record late evidence. A panicked worker gets the
    /// same explicit reconciling park as a reclaimed one so no execution
    /// disappears without a durable record.
    async fn join_workers(&self, workers: Vec<Worker>) {
        for worker in &workers {
            worker.cancellation.cancel();
        }
        for worker in workers {
            match worker.handle.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    tracing::warn!(task_id=%worker.task_id, %error, "managed native execution lost during teardown; parking for reconciliation");
                    if let Ok(task) = self.store.external_task(worker.task_id) {
                        self.store.journal_runtime_error(
                            task.agent_id,
                            shenko_store::RuntimeErrorContext {
                                kind: "native_worker_lost".into(),
                                source_module: module_path!().into(),
                                message: format!("managed native execution lost during teardown: {error}"),
                                step_no: None,
                                task_id: Some(worker.task_id),
                                extra: serde_json::json!({}),
                            },
                        );
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

    /// Upgrade handoff quiesce — upgrade seam: close admission and
    /// drain every managed execution EXCEPT the excluded orchestration
    /// workers (the caller runs inside one of them and must not join
    /// itself). Admission stays closed until `resume_after_handoff_abort`
    /// (same-generation reopen on abort) or the service is torn down /
    /// reopened by the succeeding generation.
    ///
    /// Do not abort outer futures: an outer future can own a spawn_blocking
    /// guest whose JoinHandle drop does NOT stop that guest or its receipt
    /// writer. Await actual finite workers; on timeout retain their handles
    /// and ownership so the same runtime can resume without replay.
    pub async fn quiesce_for_handoff(
        &self,
        excluded_task_ids: &[EntityId],
    ) -> Result<serde_json::Value> {
        let _guard = self.run_lock.lock().await;
        let excluded: std::collections::HashSet<EntityId> =
            excluded_task_ids.iter().copied().collect();
        for task in excluded_task_ids {
            let task = self.store.external_task(*task)?;
            anyhow::ensure!(task.kind == "update.control",
                "only an explicit update.control continuation can be excluded from handoff drain");
        }
        let apply_epoch = {
            let mut executions = self.executions.lock();
            executions.closed = true;
            executions.apply_epoch
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let ready = {
                let executions = self.executions.lock();
                executions.workers.iter().filter(|(id, _)| !excluded.contains(id))
                    .all(|(_, worker)| worker.handle.is_finished())
            };
            if ready { break; }
            anyhow::ensure!(tokio::time::Instant::now() < deadline,
                "NATIVE_HANDOFF_INCOMPLETE: actual workers still own execution; no cancellation/termination claimed");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let drained = {
            let mut executions = self.executions.lock();
            let ids = executions.workers.keys().filter(|id| !excluded.contains(id)).copied().collect::<Vec<_>>();
            ids.into_iter().filter_map(|id| executions.workers.remove(&id)).collect::<Vec<_>>()
        };
        // All handles are finished. The shared join path cannot abandon a
        // live guest here, and still parks panics with their original debt.
        self.join_workers(drained).await;
        let executions = self.executions.lock();
        Ok(serde_json::json!({
            "v": 1,
            "kind": "native_child_handoff_quiesce",
            "instance_id": self.instance_id,
            "apply_epoch": apply_epoch,
            "closed": executions.closed,
            "retained_task_ids": executions.workers.keys().collect::<Vec<_>>(),
            "excluded_task_ids": excluded_task_ids,
        }))
    }

    /// Reopen admission after a handoff was aborted. Unlike `reopen()` this
    /// does NOT bump `apply_epoch`: no new Cordis apply ran, so a pending
    /// teardown effect of this generation must not be made stale by the
    /// resume. The excluded orchestration worker is still live and stayed in
    /// the map; durable recovery reconciles the debt the quiesce left.
    pub async fn resume_after_handoff_abort(&self) -> Result<()> {
        let _guard = self.run_lock.lock().await;
        // A timeout retained live handles: do not reclassify those durable
        // claims as recovery work while their original workers still run.
        self.executions.lock().closed = false;
        Ok(())
    }

    /// Deterministic guidance from currently installed handlers, ordered by
    /// frozen kind/version. The registry supplies the callable address even
    /// when the handler's usage text omits it.
    pub fn installed_prompt_sections(&self) -> Vec<String> {
        let registry = self.registry.read();
        registry.current.iter().filter_map(|((kind, version), digest)| {
                let registered = registry.handlers.get(&(kind.clone(), *version, digest.clone()))?;
                let descriptor = &registered.descriptor;
                descriptor.prompt_section.as_ref().map(|section| {
                    format!(
                        "[delegate:{}:{}] 后接该定义的 JSON 输入。\n{section}",
                        descriptor.kind, descriptor.definition_version
                    )
                })
            })
            .collect()
    }

    /// The registered `(kind, definition_version)` pairs, ordered. This is the
    /// same registry the prompt sections come from, surfaced so self-knowledge
    /// can report what this instance can actually execute.
    pub fn installed_definitions(&self) -> Vec<(String, u32)> {
        self.registry
            .read()
            .current
            .keys()
            .cloned()
            .collect()
    }

    /// The runtime handle a finite model child receives. It is this very
    /// service, so a child calls the same registered handlers the subject
    /// can delegate to, with the same authority and the same ledger.
    pub fn tool_runtime(self: &Arc<Self>) -> Arc<dyn ChildToolRuntime> {
        Arc::new(NativeToolRuntime {
            service: self.clone(),
        })
    }

    /// The installed description of every allowed tool that is actually
    /// registered. An allowed tool the registry does not have is not
    /// offered: the child's catalog is the runtime's truth.
    pub fn describe_tools(
        &self,
        allowed: &[shenko_types::output::ModelChildTool],
    ) -> Vec<(String, u32, String)> {
        allowed
            .iter()
            .filter_map(|tool| {
                self.handler(&tool.kind, tool.definition_version)
                    .map(|registered| {
                        (
                            tool.kind.clone(),
                            tool.definition_version,
                            registered.descriptor.prompt_section.unwrap_or_default(),
                        )
                    })
            })
            .collect()
    }

    /// Execute one tool call of a finite model child as a durable native
    /// subtask.
    ///
    /// The tool runs the very handler the subject could delegate to; its own
    /// operation, attempt, evidence and T2 land on the same ledger, and the
    /// subtask is owned by the child task, so cancelling the child fences it.
    /// The call is never executed twice: an existing subtask id is read, not
    /// re-run.
    pub async fn call_tool(
        self: &Arc<Self>,
        call: &ChildToolCall,
        parent: &TaskRecord,
        authority: &AuthorityRef,
    ) -> anyhow::Result<ChildToolOutcome> {
        self.call_tool_cancellable(call, parent, authority, Default::default()).await
    }

    async fn call_tool_cancellable(
        self: &Arc<Self>,
        call: &ChildToolCall,
        parent: &TaskRecord,
        authority: &AuthorityRef,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<ChildToolOutcome> {
        ensure!(!cancellation.is_cancelled(), "child tool execution is stopping");
        let unavailable = |reason: String, result: serde_json::Value| ChildToolOutcome {
            call_id: call.call_id.clone(),
            kind: call.kind.clone(),
            definition_version: call.definition_version,
            task_id: call.task_id,
            operation_id: EntityId::nil(),
            outcome: reason,
            result: Some(result),
            evidence_ref: None,
        };
        let Some(registered) = self.handler(&call.kind, call.definition_version) else {
            return Ok(unavailable(
                "unavailable".into(),
                serde_json::json!({
                    "reason": "no registered native definition for this tool",
                    "kind": call.kind,
                    "definition_version": call.definition_version,
                }),
            ));
        };
        // An existing subtask is read, never re-run.
        if let Some(existing) = self.store.get_task(call.task_id)? {
            let result = self.store.native_task_result(call.task_id)?;
            if result.is_none() && existing.abort_request.is_none() {
                if let Some(operation_id) = memo_uuid(&existing, NATIVE_OPERATION_ID) {
                    if let Some(operation) = self.store.get_operation(operation_id)? {
                        return Self::unsettled_tool_outcome(&self.store, call, &operation);
                    }
                }
            }
            let outcome = match &result {
                Some(result) => match result.outcome {
                    TaskOutcome::Succeeded => "succeeded",
                    TaskOutcome::Cancelled => "cancelled",
                    TaskOutcome::Failed { .. } => "failed",
                    TaskOutcome::Interrupted => "unknown",
                },
                None => {
                    if existing.abort_request.is_some() {
                        "cancelled"
                    } else {
                        "unknown"
                    }
                }
            };
            return Ok(ChildToolOutcome {
                call_id: call.call_id.clone(),
                kind: call.kind.clone(),
                definition_version: call.definition_version,
                task_id: call.task_id,
                operation_id: result
                    .as_ref()
                    .map(|result| result.operation_id)
                    .unwrap_or_else(EntityId::nil),
                outcome: outcome.into(),
                result: result.as_ref().and_then(|result| result.result.clone()),
                evidence_ref: result.map(|result| result.evidence_ref),
            });
        }
        // Freeze the tool input as the subtask's sealed input object.
        let bytes = serde_json::to_vec(&call.input)?;
        let sealed = self
            .store
            .cas
            .seal_object(
                &bytes,
                "application/json",
                "model-child-tool",
                "model-child-tool-input",
                1,
            )
            .map_err(|error| {
                self.store.close_external_writer();
                error
            })?;
        let admitted = self.store.claim_child_tool_task(
            parent,
            call.task_id,
            &call.kind,
            call.definition_version,
            sealed.object,
            registered.descriptor.implementation_ref.clone(),
        );
        let Some(claim) = (match admitted {
            Ok(claim) => claim,
            Err(error) if error.is::<shenko_store::ModelChildBudgetExceeded>() => {
                return Ok(unavailable("failed".into(), serde_json::json!({"reason": error.to_string()})));
            }
            Err(error) => return Err(error),
        })
        else {
            return Ok(unavailable(
                "unknown".into(),
                serde_json::json!({
                    "reason": "the tool subtask already exists; read its durable result",
                    "kind": call.kind,
                }),
            ));
        };
        let context = CallContext {
            caller_instance_id: self.instance_id,
            role_revision: registered.descriptor.role_revision.clone(),
            task_id: Some(call.task_id),
            expected_task_revision: Some(claim.invocation.task_revision.0),
            invocation_id: Some(claim.invocation.invocation_id),
            controller_epoch: claim.invocation.epoch,
            software_generation: claim.software_generation.clone(),
            authority_refs: registered.descriptor.authority_refs.clone(),
            trace_id: EntityId::new(),
        };
        let invocation = NativeTaskInvocation {
            context,
            task_invocation: claim.invocation.clone(),
            task: claim.task.clone(),
            operation_id: claim.operation_id,
            authority: authority.clone(),
            input: call.input.clone(),
            child_tools: Some(self.tool_runtime()),
            cancellation,
        };
        let operation = match execute_native_handler(registered.handler.as_ref(), invocation).await {
            Ok(operation) => operation,
            Err(error) => {
                if error.is::<NativeTaskDeferred>()
                    && self.store.get_operation(claim.operation_id)?.is_none()
                {
                    let current = self.store.external_task(claim.task.task_id)?;
                    park_managed(&self.store, &current, "reconciling", &error.to_string(), true);
                    // This is local scheduling information, not an invented
                    // Failed/Unknown operation or a terminal tool result.
                    return Ok(unavailable("unavailable".into(), serde_json::json!({
                        "reason": error.to_string(), "kind": call.kind, "retry_pending": true,
                    })));
                }
                park_managed(
                    &self.store,
                    &claim.task,
                    "handler_failed",
                    &format!("tool handler did not produce a settled result: {error}"),
                    true,
                );
                return Ok(unavailable(
                    "failed".into(),
                    serde_json::json!({
                        "reason": format!("tool handler failed: {error}"),
                        "kind": call.kind,
                    }),
                ));
            }
        };
        ensure!(operation.operation_id == claim.operation_id && operation.task_id == claim.task.task_id,
            "handler returned a different task/operation identity");
        if operation.state != OperationState::Resolved {
            // Tool calls use the same unresolved-operation boundary as the
            // managed dispatcher. An observed-but-unsettled attempt is not
            // eligible for T2, and must not become a generic completion error
            // that hides its original evidence or encourages a new attempt.
            park_managed(&self.store, &claim.task, "reconciling",
                "armed tool operation is unresolved; query original evidence without replay", true);
            return Self::unsettled_tool_outcome(&self.store, call, &operation);
        }
        let outcome = match operation.outcome {
            Some(OperationOutcome::Succeeded) => "succeeded",
            Some(OperationOutcome::Cancelled) => "cancelled",
            Some(OperationOutcome::Failed) | Some(OperationOutcome::Interrupted) => "failed",
            None => "unknown",
        };
        // T2 binds the resolved operation to the durable result entry.
        let settled = self.store.complete_native_task(&claim, &operation)?;
        Ok(ChildToolOutcome {
            call_id: call.call_id.clone(),
            kind: call.kind.clone(),
            definition_version: call.definition_version,
            task_id: call.task_id,
            operation_id: operation.operation_id,
            outcome: outcome.into(),
            result: settled.as_ref().and_then(|result| result.result.clone()),
            evidence_ref: settled.as_ref().map(|result| result.evidence_ref.clone()),
        })
    }

    /// An unresolved operation can have genuine E1 without an accepted T2.
    /// Return its original observation on both first delivery and later reads;
    /// never imply that missing T2 means no request ran or no evidence exists.
    /// This read needs only the durable Store, not an installed handler or a
    /// live runtime, so transcript reconstruction cannot replay an effect.
    pub(crate) fn unsettled_tool_outcome(
        store: &Store,
        call: &ChildToolCall,
        operation: &OperationRecord,
    ) -> Result<ChildToolOutcome> {
        ensure!(operation.task_id == call.task_id, "unresolved tool operation belongs to another task");
        let attempt = store.external_operation_attempt(operation.operation_id)?;
        let evidence = attempt.final_receipt_id.map(|id| store.external_evidence(id)).transpose()?;
        if let Some(evidence) = &evidence {
            ensure!(evidence.operation_id == Some(operation.operation_id) && evidence.attempt_id == Some(attempt.attempt_id),
                "unresolved tool receipt belongs to another operation/attempt");
        }
        let result = match evidence.as_ref().and_then(|receipt| receipt.normalized_ref.as_ref()) {
            Some(object) if object.bytes <= 512 * 1024 => {
                let bytes = store.cas.read_object(&object.digest)?;
                ensure!(bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest,
                    "unresolved native evidence identity mismatch");
                Some(serde_json::from_slice(&bytes)?)
            }
            Some(object) => Some(serde_json::json!({"status":"unknown","normalized_result_ref":object,
                "content_included":false,"reason":"normalized observation exceeds the inline result bound"})),
            None => None,
        };
        Ok(ChildToolOutcome { call_id: call.call_id.clone(), kind: call.kind.clone(), definition_version: call.definition_version,
            task_id: call.task_id, operation_id: operation.operation_id, outcome: "unknown".into(),
            result, evidence_ref: evidence.map(|receipt| receipt.raw_ref) })
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
    fn process(self: &Arc<Self>, task: &TaskRecord) -> Result<Visit> {
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
        let operation_id = memo_uuid(task, NATIVE_OPERATION_ID);
        let existing = match operation_id {
            Some(id) => self.store.get_operation(id)?,
            None => None,
        };
        if memo_flag(task, NATIVE_CHILD)
            && let Some(operation) = existing.as_ref().filter(|operation| operation.state == OperationState::Resolved)
        {
                // E1 already owns the conclusive result. Generic T2 does not
                // execute a handler or need to reinterpret its old receipt.
                // Keep the ORIGINAL implementation identity through settlement.
                let Some(claim) = self.store.claim_native_task(task.task_id, task.implementation_ref.clone(), true)? else {
                    return Ok(Visit::Skipped);
                };
                return Ok(if self.store.complete_native_task(&claim, operation)?.is_some() {
                    Visit::Progressed
                } else { Visit::Skipped });
        }
        let registered = match self.frozen_handler(task) {
            Some(registered) => Some(registered),
            None => match existing.as_ref() {
                Some(operation) => self.recovery_handler(task, operation)?,
                None => None,
            },
        };
        let registered = match registered {
            Some(registered) => registered,
            None => {
                self.store.park_native_task(
                    task.task_id,
                    task.record_revision,
                    if memo_flag(task, NATIVE_CHILD) { "implementation_unavailable" } else { "handler_unavailable" },
                    "the exact frozen executable is not installed; retain its original operations and recovery artifact",
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
                let implementation = if memo_flag(task, NATIVE_CHILD) {
                    task.implementation_ref.clone()
                } else {
                    registered.descriptor.implementation_ref.clone()
                };
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
            child_tools: Some(self.tool_runtime()),
            cancellation: Default::default(),
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
        self.store.native_task_input(task)
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
        // A rejected park CAS is itself a runtime error the subject can read.
        store.journal_runtime_error(
            task.agent_id,
            shenko_store::RuntimeErrorContext {
                kind: "native_park".into(),
                source_module: module_path!().into(),
                message: format!("managed park rejected for {reason}: {error}"),
                step_no: None,
                task_id: Some(task.task_id),
                extra: serde_json::json!({ "reason": reason }),
            },
        );
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
    cancellation: tokio_util::sync::CancellationToken,
) {
    match job {
        WorkerJob::Execute { claim, invocation } => {
            let task_id = invocation.task.task_id;
            let agent_id = invocation.task.agent_id;
            let operation = match execute_native_handler(registered.handler.as_ref(), invocation).await {
                Ok(operation) => operation,
                Err(error) => {
                    if error.is::<NativeTaskDeferred>() {
                        match store.get_operation(claim.operation_id) {
                            Ok(None) => {
                                if let Some(current) = fetch_task(&store, task_id) {
                                    // The existing durable retry timer and scan
                                    // cursor release this worker without T1/T2.
                                    // A retry reclaims the same operation ID;
                                    // an actual operation always uses Settle.
                                    park_managed(&store, &current, "reconciling", &error.to_string(), true);
                                }
                                return;
                            }
                            Ok(Some(_)) => {} // An armed operation is never a fresh retry.
                            Err(read_error) => {
                                tracing::warn!(task_id=%task_id, %read_error,
                                    "cannot prove pre-T1 deferral; retaining the durable claim for recovery");
                                return;
                            }
                        }
                    }
                    // the handler failure is also a runtime fact
                    // the owning subject can read; journaling is fail-soft.
                    store.journal_runtime_error(
                        agent_id,
                        shenko_store::RuntimeErrorContext {
                            kind: "native_execute".into(),
                            source_module: module_path!().into(),
                            message: format!("native handler did not produce a settled result: {error}"),
                            step_no: None,
                            task_id: Some(task_id),
                            extra: serde_json::json!({}),
                        },
                    );
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
                store.journal_runtime_error(
                    agent_id,
                    shenko_store::RuntimeErrorContext {
                        kind: "native_handler_contract".into(),
                        source_module: module_path!().into(),
                        message: "handler returned an operation with a different task/operation identity".into(),
                        step_no: None,
                        task_id: Some(task_id),
                        extra: serde_json::json!({}),
                    },
                );
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
            // The handler owns propagation and joining. An outer select would
            // drop a reader that still owns a guest or an E1 writer.
            let result = registered.handler.reconcile_with_cancellation(
                &task, operation_id, cancellation,
            ).await;
            let operation = match result {
                Ok(Some(operation)) => operation,
                Ok(None) => return,
                Err(error) => {
                    store.journal_runtime_error(
                        task.agent_id,
                        shenko_store::RuntimeErrorContext {
                            kind: "native_reconcile".into(),
                            source_module: module_path!().into(),
                            message: format!("original operation remains unresolved: {error}"),
                            step_no: None,
                            task_id: Some(task.task_id),
                            extra: serde_json::json!({ "operation_id": operation_id.to_string() }),
                        },
                    );
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
            if operation.task_id != task.task_id || operation.operation_id != operation_id {
                park_managed(
                    &store, &task, "handler_contract_violation",
                    "reconciliation returned a different task/operation identity", false,
                );
                return;
            }
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
                    store.journal_runtime_error(
                        task.agent_id,
                        shenko_store::RuntimeErrorContext {
                            kind: "native_settlement".into(),
                            source_module: module_path!().into(),
                            message: format!("settlement-only claim could not be made: {error}"),
                            step_no: None,
                            task_id: Some(task.task_id),
                            extra: serde_json::json!({ "operation_id": operation_id.to_string() }),
                        },
                    );
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
                    store.journal_runtime_error(
                        task.agent_id,
                        shenko_store::RuntimeErrorContext {
                            kind: "native_settlement".into(),
                            source_module: module_path!().into(),
                            message: format!("resolved operation could not be committed: {error}"),
                            step_no: None,
                            task_id: Some(task.task_id),
                            extra: serde_json::json!({ "operation_id": operation_id.to_string() }),
                        },
                    );
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
    /// A destructor cannot await a guest. Signal cancellation but retain the
    /// worker future so it can join the real executor and preserve late E1.
    /// Only `close_and_join` is a completed shutdown barrier; dropping this
    /// fallback does not claim that effects have stopped.
    fn drop(&mut self) {
        let executions = self.executions.get_mut();
        executions.closed = true;
        for (_, worker) in executions.workers.drain() {
            worker.cancellation.cancel();
        }
    }
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
        // recovered into query-only reconciliation. On first publication,
        // recovery belongs to the owning composition root, not catalog discovery.
        let epoch = service
            .reopen()
            .await
            .map_err(std::io::Error::other)?;
        let _publication = ctx
            .provide(service.clone())
            .map_err(std::io::Error::other)?;
        // Worker lifetime is bound to the runtime, not to the service Arc:
        // shutdown closes admission and signals/joins every real execution
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
impl NativeTaskHandlerPlugin {
    /// Install only the historical origin's receipt reader in this Cordis
    /// generation. It grants no fresh definition, capability, help or tool.
    /// The handler must reject execute and validate its original contract and
    /// frozen payload before querying; only Settle can select this entry.
    pub fn install_recovery(
        ctx: Context,
        handler: Arc<dyn NativeTaskHandler>,
    ) -> Result<(), std::io::Error> {
        let service = ctx
            .try_service::<NativeChildService>()
            .map_err(std::io::Error::other)?;
        let (key, epoch) = service
            .install_handler(handler, true)
            .map_err(std::io::Error::other)?;
        ctx.effect_sync(move || service.uninstall(&key, epoch))
            .map_err(std::io::Error::other)?;
        Ok(())
    }
}

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
        // Live registration: while this definition is installed the runtime's
        // discovery view names it, its effect contracts and its capabilities.
        // The withdrawal runs in the same generation teardown, so an
        // uninstall cannot leave a stale provider or capability behind.
        register_native_definition(&ctx, handler.as_ref())?;
        Ok(())
    }
}

/// Publish one installed native definition in the live runtime registry.
///
/// The provider view names the exact implementation, the effect
/// contracts of the definition's operations, the capabilities it offers and
/// the usability package of every capability. Everything registered
/// here is withdrawn in the same generation teardown, so an uninstall cannot
/// leave a stale provider, capability or help document behind.
fn register_native_definition(
    ctx: &Context,
    handler: &dyn NativeTaskHandler,
) -> Result<(), std::io::Error> {
    let descriptor = handler.descriptor().map_err(std::io::Error::other)?;
    let runtime = match ctx.try_service::<shenko_kernel::runtime::PluginRuntime>() {
        Ok(runtime) => runtime,
        Err(error) => {
            // The definition itself is installed; the live discovery view is
            // not. This is an assembly fact, not a silent success.
            tracing::warn!(
                kind = %descriptor.kind,
                %error,
                "native definition installed without a live runtime registry"
            );
            return Ok(());
        }
    };
    let implementation = descriptor.implementation();
    let provider_id = format!("native.{}", descriptor.kind);
    let declarations = handler.capabilities();
    let operations = handler
        .effect_contracts()
        .map_err(std::io::Error::other)?
        .into_iter()
        .map(|contract| shenko_types::plugin_runtime::OperationEffectView {
            operation_name: contract.operation_name.clone(),
            effect_contract: contract,
        })
        .collect();
    let provider = runtime
        .register_provider(shenko_kernel::runtime::ProviderRegistration {
            provider_id: provider_id.clone(),
            kind: "native.task".into(),
            version: descriptor.definition_version.to_string(),
            plugin_id: descriptor.implementation_ref.plugin_id,
            implementation: implementation.clone(),
            description: descriptor.prompt_section.clone().unwrap_or_else(|| {
                format!("native task definition {}", descriptor.kind)
            }),
            operations,
            capabilities: declarations
                .iter()
                .map(|declaration| declaration.capability_id.clone())
                .collect(),
            health: shenko_types::plugin_runtime::ProviderHealth::Installed,
        })
        .map_err(std::io::Error::other)?;
    let mut capability_handles = Vec::new();
    let mut help_handles = Vec::new();
    for declaration in &declarations {
        let handle = runtime
            .register_capability(shenko_kernel::runtime::CapabilityRegistration {
                capability_id: declaration.capability_id.clone(),
                version: declaration.version.clone(),
                description: declaration.description.clone(),
                plugin_id: descriptor.implementation_ref.plugin_id,
                provider_id: Some(provider_id.clone()),
                enabled: true,
            })
            .map_err(std::io::Error::other)?;
        capability_handles.push(handle);
        // usability package: what the capability is for, when it
        // applies and where its own limits are. The text comes from the
        // installed implementation, not from a product feature list.
        let help = runtime
            .register_help(shenko_kernel::runtime::HelpRegistration {
                capability_id: declaration.capability_id.clone(),
                version: declaration.version.clone(),
                purpose: declaration.description.clone(),
                key_methods: declaration.usage.clone().into_iter().collect(),
                applies_when: vec![format!(
                    "the definition {}@{} is installed in this runtime",
                    descriptor.kind, descriptor.definition_version
                )],
                does_not_apply_when: vec![format!(
                    "the definition {} is not installed",
                    descriptor.kind
                )],
                schema_help: Some(format!(
                    "native task definition {} schema version {}",
                    descriptor.kind, descriptor.definition_version
                )),
                limitations: Vec::new(),
                recovery: None,
                implementation: implementation.clone(),
            })
            .map_err(std::io::Error::other)?;
        help_handles.push(help);
    }
    let teardown_runtime = runtime.clone();
    let teardown_provider = provider.clone();
    ctx.effect_sync(move || {
        for handle in &capability_handles {
            teardown_runtime.withdraw_capability(handle);
        }
        for handle in &help_handles {
            teardown_runtime.withdraw_help(handle);
        }
        teardown_runtime.withdraw_provider(&teardown_provider);
    })
    .map_err(std::io::Error::other)?;
    Ok(())
}
