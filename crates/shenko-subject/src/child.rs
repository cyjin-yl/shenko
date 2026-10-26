//! Native finite-child registry and dispatcher on the pinned Cordis lifecycle.
//! Model output can request a definition, but only this typed Service creates
//! CallContext/authority. Natural-language goals are not executable definitions.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use cordis_core::{Context, InjectSpec, Plugin, Service};
use parking_lot::RwLock;
use shenko_store::{
    Store, TaskCancellation,
    child::{
        NATIVE_CHILD, NATIVE_INPUT, NATIVE_OPERATION_ID, NATIVE_RECOVERY, memo_flag, memo_uuid,
        park_reason,
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
use std::{borrow::Cow, collections::BTreeMap, convert::Infallible, sync::Arc};

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

    fn install(
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
    pub async fn run_once(&self) -> Result<usize> {
        let _guard = self.run_lock.lock().await;
        let candidates = self.store.native_task_candidates(32)?;
        let mut progressed = 0;
        for task in candidates {
            progressed += usize::from(self.process(task).await?);
        }
        Ok(progressed)
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

    async fn process(&self, task: TaskRecord) -> Result<bool> {
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
            let mut operation = existing.expect("checked above");
            if operation.state != OperationState::Resolved {
                match registered.handler.reconcile(&task, operation_id).await {
                    Ok(observed) => operation = observed,
                    Err(error) => {
                        self.park_current(
                            task.task_id,
                            "reconciling",
                            &format!("original operation remains unresolved: {error}"),
                            true,
                        )?;
                        return Ok(true);
                    }
                }
            }
            if operation.state != OperationState::Resolved {
                self.park_current(
                    task.task_id,
                    "reconciling",
                    "original armed operation has no conclusive outcome; no replay",
                    true,
                )?;
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
        let operation = match registered.handler.execute(invocation).await {
            Ok(operation) => operation,
            Err(error) => {
                let has_operation = self.store.get_operation(claim.operation_id)?.is_some();
                self.park_current(
                    task.task_id,
                    if has_operation {
                        "reconciling"
                    } else {
                        "handler_failed"
                    },
                    &format!("native handler did not produce a settled result: {error}"),
                    has_operation,
                )?;
                return Ok(true);
            }
        };
        if operation.task_id != task.task_id || operation.operation_id != claim.operation_id {
            self.park_current(
                task.task_id,
                "handler_contract_violation",
                "handler returned an operation with a different task/operation identity",
                false,
            )?;
            return Ok(true);
        }
        if operation.state == OperationState::Resolved {
            Ok(self
                .store
                .complete_native_task(&claim, &operation)?
                .is_some())
        } else {
            self.park_current(
                task.task_id,
                "reconciling",
                "armed native operation is unresolved; query original evidence before any replay",
                true,
            )?;
            Ok(true)
        }
    }

    fn park_current(
        &self,
        task_id: EntityId,
        reason: &str,
        detail: &str,
        retry: bool,
    ) -> Result<()> {
        let current = self.store.external_task(task_id)?;
        self.store
            .park_native_task(task_id, current.record_revision, reason, detail, retry)?;
        Ok(())
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
                ensure!(decision.schema_version == 4, "unsupported_decision_schema");
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
