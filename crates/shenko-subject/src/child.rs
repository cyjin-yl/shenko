//! Native finite-child registry and dispatcher on the pinned Cordis lifecycle.
//! Model output can request a definition, but only this typed Service creates
//! CallContext/authority. Natural-language goals are not executable definitions.
//!
//! Dispatch is claim-driven: `run_once` only reads durable candidates, performs
//! the synchronous CAS claim and spawns a bounded managed execution. Slow or
//! unknown handlers no longer hold the dispatcher pass hostage, so a finished
//! short child can project its T2/result while a long child is still running.
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
    /// of truth for recovery.
    executions: parking_lot::Mutex<HashMap<EntityId, Worker>>,
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
            executions: parking_lot::Mutex::new(HashMap::new()),
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

    /// One bounded pass over durable candidates. Duplicate wakeups only reread
    /// this state; CAS determines whether a handler/attempt may be created.
    /// Finished managed executions are reclaimed first, so a completed short
    /// child projects its result even while another child keeps running.
    /// Handler waits themselves happen inside spawned executions, never under
    /// this pass's lock.
    pub async fn run_once(self: &Arc<Self>) -> Result<usize> {
        let _guard = self.run_lock.lock().await;
        self.reclaim_finished().await;
        let candidates = self.store.native_task_candidates(32)?;
        let mut progressed = 0;
        for task in candidates {
            progressed += usize::from(self.process(task)?);
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
                .iter()
                .filter(|(_, worker)| worker.handle.is_finished())
                .map(|(task_id, _)| *task_id)
                .collect();
            ids.into_iter()
                .filter_map(|task_id| executions.remove(&task_id))
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

    /// Spawn a bounded managed execution for a task whose claim/settlement is
    /// already durable. Returns false when the execution budget is full or a
    /// worker already owns this task; the caller leaves the durable state
    /// untouched and retries on a later pass.
    fn worker_available(&self, task_id: EntityId) -> bool {
        let executions = self.executions.lock();
        executions.len() < MAX_INFLIGHT_EXECUTIONS && !executions.contains_key(&task_id)
    }

    fn spawn_execution(
        &self,
        task_id: EntityId,
        registered: RegisteredHandler,
        job: WorkerJob,
    ) {
        let store = self.store.clone();
        let handle = tokio::spawn(async move {
            drive_execution(store, registered, job).await;
        });
        self.executions.lock().insert(task_id, Worker { task_id, handle });
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

    /// Advance one durable candidate by at most one synchronous CAS step. Any
    /// handler wait (fresh `execute` or recovery `reconcile`) is delegated to a
    /// spawned managed execution; this pass itself never awaits a handler.
    fn process(&self, task: TaskRecord) -> Result<bool> {
        if task.memos.contains_key(COGNITION_IMPORTED) {
            // Do not even rewrite the import into an ordinary retryable park.
            // Only an explicit local continuation may remove this marker.
            return Ok(false);
        }
        if matches!(task.state, TaskState::Completing { .. }) {
            return Ok(self.store.finish_native_completing(task.task_id)?.is_some());
        }
        if task.is_terminal() || task.is_running() {
            return Ok(false);
        }
        if !task.memos.contains_key(NATIVE_INPUT) {
            if park_reason(&task) != Some("definition_unavailable") {
                self.store.park_native_task(
                    task.task_id,
                    task.record_revision,
                    "definition_unavailable",
                    "natural-language delegation has no deterministic native definition",
                    false,
                )?;
                return Ok(true);
            }
            return Ok(false);
        }
        if task.abort_request.is_some() {
            // Cancellation already fenced new claims. EvidenceIngress remains
            // independent; no current result/outbox is derived from late facts.
            return Ok(false);
        }
        if let DispatchEligibility::Parked {
            until_utc: Some(until),
            ..
        } = &task.dispatch_eligibility
        {
            if until > &chrono::Utc::now() {
                return Ok(false);
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
                return Ok(true);
            }
        };
        if memo_flag(&task, NATIVE_CHILD)
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
            return Ok(true);
        }
        let operation_id = memo_uuid(&task, NATIVE_OPERATION_ID);
        let existing = match operation_id {
            Some(id) => self.store.get_operation(id)?,
            None => None,
        };
        let recovery = memo_flag(&task, NATIVE_RECOVERY) || existing.is_some();
        if recovery && existing.is_some() {
            let operation_id = operation_id.expect("checked above");
            let operation = existing.expect("checked above");
            if operation.state != OperationState::Resolved {
                // The original provider query runs in a managed execution so a
                // slow or unreachable handle cannot hold the dispatcher pass.
                if !self.worker_available(task.task_id) {
                    return Ok(false);
                }
                let implementation = registered.descriptor.implementation_ref.clone();
                self.spawn_execution(
                    task.task_id,
                    registered,
                    WorkerJob::Settle {
                        task,
                        operation_id,
                        implementation,
                    },
                );
                return Ok(true);
            }
            let Some(claim) = self.store.claim_native_task(
                task.task_id,
                registered.descriptor.implementation_ref.clone(),
                true,
            )?
            else {
                return Ok(false);
            };
            return Ok(self
                .store
                .complete_native_task(&claim, &operation)?
                .is_some());
        }
        if matches!(park_reason(&task), Some(reason) if !matches!(reason,
            "handler_unavailable" | "reconciling") && !recovery)
        {
            return Ok(false);
        }
        // The claim must never be created without a spawned owner to settle
        // it; capacity is checked before the CAS transitions Pending→Running.
        if !self.worker_available(task.task_id) {
            return Ok(false);
        }
        let Some(claim) = self.store.claim_native_task(
            task.task_id,
            registered.descriptor.implementation_ref.clone(),
            false,
        )?
        else {
            return Ok(false);
        };
        if claim.software_generation != self.generation {
            self.store.park_native_task(
                task.task_id,
                claim.task.record_revision,
                "generation_unavailable",
                "registered runtime generation does not match the sealed task head",
                false,
            )?;
            return Ok(true);
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
                return Ok(true);
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
        self.spawn_execution(
            task_id,
            registered,
            WorkerJob::Execute { claim, invocation },
        );
        Ok(true)
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
                // S1 decision. New waits use schema 6; both bind the same native
                // delegation field without rewriting the historical CAS object.
                ensure!(matches!(decision.schema_version, 4 | 6), "unsupported_decision_schema");
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
    fn drop(&mut self) {
        for (_, worker) in self.executions.get_mut().drain() {
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
    type ApplyError = cordis_core::service::ServicePublishError;

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
        let _publication = ctx.provide(service.clone())?;
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
