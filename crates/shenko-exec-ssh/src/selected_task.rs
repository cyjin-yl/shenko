//! Explicit agent choice bound to the existing SSH T1, not a new dispatcher.
//!
//! Only an installed authenticated child-side adapter may call this API. Its
//! invocation/authority and registered node-to-target mapping MUST NOT come
//! from model arguments. This module validates durable bindings; it does not
//! grant permissions, select a replacement node, or expose administrative RPC.

use crate::{SshAction, SshBinding, SshObservation, SshTaskRunner, task_runner::FrozenInput};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use shenko_node::{CapabilityKnowledge, Node, NodeSelectionOutcome, NodeSelectionReceipt};
use shenko_store::{TaskInvocation, child::{COGNITION_IMPORTED, NATIVE_ASSIGNMENT, NATIVE_OPERATION_ID, NATIVE_SETTLEMENT_ONLY}};
use shenko_types::{ids::EntityId, object::Digest, operation::AuthorityRef, task::{DispatchEligibility, ExecutionAssignment, RunMode, TaskRecord, TaskState}};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectedSshUnavailable {
    TaskChanged,
    TaskNotAdmissible,
    InvocationChanged,
    AssignmentChanged,
    AuthorityMismatch,
    OperationMismatch,
    SelectionMissing,
    SelectionNotRecorded,
    TargetBindingMismatch,
    NodeNotEnrolled,
    ObservationUnknown,
    ObservationOversized,
    ObservationChanged,
    FrozenSelectionConflict,
}

#[derive(Debug)]
pub enum SelectedSshOutcome {
    Observed(SshObservation),
    Unavailable(SelectedSshUnavailable),
}

/// Included in the SAME frozen input whose digest T1 and the remote receipt
/// seal already bind. No separate CAS preparation or second admission ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskBoundNodeSelection {
    pub(super) schema: String,
    pub(super) invocation: FrozenTaskInvocation,
    pub(super) authority: AuthorityRef,
    pub(super) operation_id: EntityId,
    pub(super) registered_node_id: EntityId,
    pub(super) receipt: NodeSelectionReceipt,
}

/// Preserve the original reader after TaskInvocation gained runtime generation.
/// A legacy invocation has UNKNOWN generation, not the current/default one.
/// It is readable only for the existing operation's query/reconcile path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum FrozenTaskInvocation {
    GenerationBound(TaskInvocation),
    Legacy(LegacyTaskInvocation),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LegacyTaskInvocation {
    task_id: EntityId,
    task_revision: shenko_types::ids::TaskRevision,
    record_revision: shenko_types::ids::RecordRevision,
    invocation_id: EntityId,
    epoch: shenko_types::ids::ControllerEpoch,
    worker_id: EntityId,
    node_id: EntityId,
}

impl TaskBoundNodeSelection {
    pub(super) fn ensure_readable(&self) -> Result<()> {
        ensure!(matches!((self.schema.as_str(), &self.invocation),
            ("ssh-task-node-selection.v1", FrozenTaskInvocation::Legacy(_))
            | ("ssh-task-node-selection.v2", FrozenTaskInvocation::GenerationBound(_))),
            "unsupported selected SSH invocation schema");
        Ok(())
    }

    pub(super) fn ensure_fresh(&self) -> Result<()> {
        ensure!(self.schema == "ssh-task-node-selection.v2"
            && matches!(&self.invocation, FrozenTaskInvocation::GenerationBound(_)),
            "legacy selected SSH input is query-only");
        Ok(())
    }
}

pub(super) fn check_task(
    task: &TaskRecord,
    ctx: &TaskInvocation,
    authority: &AuthorityRef,
    operation_id: EntityId,
) -> Result<Option<SelectedSshUnavailable>> {
    use SelectedSshUnavailable as U;
    if task.task_id != ctx.task_id || task.task_revision != ctx.task_revision || task.record_revision != ctx.record_revision {
        return Ok(Some(U::TaskChanged));
    }
    if task.abort_request.is_some() || !matches!(task.dispatch_eligibility, DispatchEligibility::Ready)
        || task.memos.contains_key(NATIVE_SETTLEMENT_ONLY) || task.memos.contains_key(COGNITION_IMPORTED) {
        return Ok(Some(U::TaskNotAdmissible));
    }
    if !matches!(task.state, TaskState::Running { invocation_id, epoch, mode: RunMode::Run, .. }
        if invocation_id == ctx.invocation_id && epoch == ctx.epoch) {
        return Ok(Some(U::InvocationChanged));
    }
    let Some(assignment) = task.memos.get(NATIVE_ASSIGNMENT) else { return Ok(Some(U::AssignmentChanged)) };
    let assignment: ExecutionAssignment = serde_json::from_value(assignment.clone())?;
    if assignment.worker_id != ctx.worker_id || assignment.node_id != ctx.node_id
        || assignment.invocation_id != ctx.invocation_id || assignment.epoch != ctx.epoch
        || assignment.implementation_ref != task.implementation_ref {
        return Ok(Some(U::AssignmentChanged));
    }
    // Match the current native dispatcher contract; a future owner changing
    // that contract must adapt this check, not silently relax it.
    if authority.delegation_id != task.task_id || authority.delegation_revision.0 != task.task_revision.0
        || authority.decision_ref != task.created_by_step_id.unwrap_or(ctx.invocation_id) {
        return Ok(Some(U::AuthorityMismatch));
    }
    if task.memos.get(NATIVE_OPERATION_ID).and_then(|v| v.as_str()) != Some(operation_id.to_string().as_str()) {
        return Ok(Some(U::OperationMismatch));
    }
    Ok(None)
}

impl SshTaskRunner<'_> {
    /// Execute one explicitly chosen, already-authorized registered target.
    ///
    /// The caller resolves `registered_node_id` and `binding` from trusted
    /// installation state and enforces its resource/permission policy. Selection
    /// is evidence, never authority. Changed observations return a reason for
    /// agent re-decision. T1 still atomically checks the current task invocation.
    /// There is no atomic transaction across Node observations and Store T1;
    /// an advertisement is not a live capacity/permission lease.
    ///
    /// A repeated already-armed operation is query-only using its ORIGINAL
    /// frozen selection, even if node enrollment/advertisements changed. A
    /// conflicting retry is refused, never rebound or sent again. This retry
    /// must retain its original invocation; a newly claimed recovery worker
    /// continues to use the existing authorized `query`/reconcile path.
    pub async fn execute_selected(
        &self,
        node: &std::sync::Mutex<Node>,
        ctx: &TaskInvocation,
        operation_id: EntityId,
        logical_key: &str,
        authority: AuthorityRef,
        decision_id: EntityId,
        registered_node_id: EntityId,
        binding: SshBinding,
        action: SshAction,
        timeout_secs: u64,
    ) -> Result<SelectedSshOutcome> {
        use SelectedSshUnavailable as U;
        let task = self.store.external_task(ctx.task_id)?;
        // This memo is written in the same T1 transaction as the operation.
        // Do not turn a lost commit response into a new selection/admission.
        if let Some(last) = task.memos.get("external:last_operation") {
            if last.get("operation_id").and_then(|v| v.as_str()) != Some(operation_id.to_string().as_str()) {
                return Ok(SelectedSshOutcome::Unavailable(U::OperationMismatch));
            }
            let op = self.store.external_operation(operation_id)?;
            let bytes = self.store.cas.read_object(&op.final_input_ref.digest)?;
            ensure!(Digest::blake3(&bytes) == op.input_digest, "frozen SSH input digest mismatch");
            let frozen: FrozenInput = serde_json::from_slice(&bytes)?;
            if !matches_original(&op, &frozen, ctx, logical_key, &authority, decision_id, registered_node_id, &binding, &action)? {
                return Ok(SelectedSshOutcome::Unavailable(U::FrozenSelectionConflict));
            }
            return Ok(SelectedSshOutcome::Observed(self.query(operation_id, timeout_secs).await?));
        }
        if let Some(reason) = check_task(&task, ctx, &authority, operation_id)? {
            return Ok(SelectedSshOutcome::Unavailable(reason));
        }
        // Release the Node lock before T1 or any network await.
        let receipt = {
            let mut node = node.lock().map_err(|_| anyhow::anyhow!("node observation lock poisoned"))?;
            let Some(receipt) = node.node_selection_receipt(decision_id)? else {
                return Ok(SelectedSshOutcome::Unavailable(U::SelectionMissing));
            };
            if !matches!(receipt.outcome, NodeSelectionOutcome::Recorded) {
                return Ok(SelectedSshOutcome::Unavailable(U::SelectionNotRecorded));
            }
            if receipt.selection.node_id != registered_node_id || binding.target_id.is_empty() {
                return Ok(SelectedSshOutcome::Unavailable(U::TargetBindingMismatch));
            }
            let unavailable = match node.node_choice_candidate(registered_node_id)? {
                None => Some(U::NodeNotEnrolled),
                Some(candidate) => match candidate.knowledge {
                    CapabilityKnowledge::Unknown => Some(U::ObservationUnknown),
                    CapabilityKnowledge::Oversized => Some(U::ObservationOversized),
                    CapabilityKnowledge::Observed { observation } => {
                        (observation.profile_digest != receipt.selection.observed_profile_digest).then_some(U::ObservationChanged)
                    }
                }
            };
            if let Some(reason) = unavailable { return Ok(SelectedSshOutcome::Unavailable(reason)); }
            receipt
        };
        let selection = TaskBoundNodeSelection {
            schema: "ssh-task-node-selection.v2".into(),
            invocation: FrozenTaskInvocation::GenerationBound(ctx.clone()), authority: authority.clone(),
            operation_id, registered_node_id, receipt,
        };
        // Existing T1 stores the complete frozen input, enforces task fencing,
        // and either arms once or reconciles the already armed original. No
        // Node admin Create/Execute RPC is involved.
        Ok(SelectedSshOutcome::Observed(self.execute_frozen(ctx, operation_id, logical_key, authority,
            FrozenInput { binding, action, selection: Some(selection) }, timeout_secs).await?))
    }
}

fn matches_original(
    op: &shenko_types::operation::OperationRecord,
    frozen: &FrozenInput,
    ctx: &TaskInvocation,
    logical_key: &str,
    authority: &AuthorityRef,
    decision_id: EntityId,
    registered_node_id: EntityId,
    binding: &SshBinding,
    action: &SshAction,
) -> Result<bool> {
    let Some(selected) = &frozen.selection else { return Ok(false) };
    let FrozenTaskInvocation::GenerationBound(invocation) = &selected.invocation else { return Ok(false) };
    Ok(op.operation_name == "ssh.native.v1" && op.task_id == ctx.task_id && op.logical_key == logical_key
        && selected.schema == "ssh-task-node-selection.v2" && selected.operation_id == op.operation_id
        && invocation.task_id == ctx.task_id && selected.registered_node_id == registered_node_id
        && serde_json::to_value(invocation)? == serde_json::to_value(ctx)?
        && selected.receipt.selection.decision_id == decision_id
        && selected.receipt.selection.node_id == registered_node_id
        && serde_json::to_value(&op.authority_ref)? == serde_json::to_value(authority)?
        && serde_json::to_value(&selected.authority)? == serde_json::to_value(authority)?
        && serde_json::to_value(&frozen.binding)? == serde_json::to_value(binding)?
        && serde_json::to_value(&frozen.action)? == serde_json::to_value(action)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_node::{Identity, NodeSelection, Request};
    use crate::{SelectedTargetRequirements, SshProviderObjectRef, SshTargetReferenceDescriptor, TargetReferenceUnavailable};
    use shenko_store::Store;
    use shenko_types::{commit::CommitRef, ids::*, store::WriteBatch, task::*};
    use std::path::PathBuf;

    struct Fixture {
        _root: PathBuf,
        store: Store,
        node: std::sync::Mutex<Node>,
        ctx: TaskInvocation,
        authority: AuthorityRef,
        operation_id: EntityId,
        decision_id: EntityId,
        target_node: EntityId,
        subject: Identity,
        binding: SshBinding,
    }

    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!("shenko-selected-ssh-{}", EntityId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::open(root.join("store.sqlite3"), root.join("cas")).unwrap();
        let mut node = Node::open(&root.join("node")).unwrap();
        let target_node = EntityId::new();
        let subject = Identity { agent_id: EntityId::new(), branch_id: EntityId::new(), origin_proof: Digest::blake3(b"selected-ssh-fixture-origin") };
        node.handle(Request::Initialize { node_id: target_node, identity: Some(subject.clone()) }).unwrap();
        let profile = node.capability_profile().unwrap();
        let decision_id = EntityId::new();
        node.record_node_selection(NodeSelection {
            decision_id, node_id: target_node,
            observed_profile_digest: Digest::blake3(&serde_json::to_vec(&profile).unwrap()),
            reason: "explicit fixture choice, not a permission grant".into(),
        }).unwrap();
        let ctx = TaskInvocation {
            task_id: EntityId::new(), task_revision: TaskRevision(1), record_revision: RecordRevision(1),
            invocation_id: EntityId::new(), epoch: ControllerEpoch(1),
            software_generation: RuntimeGeneration::new("selected-ssh-fixture"),
            worker_id: store.capabilities.incarnation, node_id: store.ledger.ledger_id(),
        };
        let authority = AuthorityRef { delegation_id: ctx.task_id, delegation_revision: RecordRevision(1), decision_ref: EntityId::new() };
        let operation_id = EntityId::new();
        let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
        let input = store.cas.seal_object(b"fixture input", "text/plain", "fixture", "fixture", 1).unwrap().object;
        let implementation = ImplementationRef { plugin_id: EntityId::new(), artifact_digest: Digest::blake3(b"fixture implementation"), registry_revision: 1 };
        let assignment = ExecutionAssignment { worker_id: ctx.worker_id, node_id: ctx.node_id, invocation_id: ctx.invocation_id, epoch: ctx.epoch, implementation_ref: implementation.clone(), assigned_commit: zero };
        let mut memos = serde_json::Map::new();
        memos.insert(NATIVE_ASSIGNMENT.into(), serde_json::to_value(assignment).unwrap());
        memos.insert(NATIVE_OPERATION_ID.into(), operation_id.to_string().into());
        let conversation_id = EntityId::new();
        let task = TaskRecord {
            record_schema_version: 1, task_id: ctx.task_id, task_revision: ctx.task_revision,
            record_revision: ctx.record_revision, agent_id: subject.agent_id, conversation_id,
            owner: TaskOwner::Conversation { id: conversation_id }, background: false,
            kind: "fixture.selected-ssh".into(), definition_version: 1, implementation_ref: implementation,
            input_ref: input.clone(), state: TaskState::Running { checkpoint_ref: None, invocation_id: ctx.invocation_id, mode: RunMode::Run, epoch: ctx.epoch },
            abort_request: None, memos, residual_case_ids: vec![], created_by_step_id: Some(authority.decision_ref),
            dispatch_eligibility: DispatchEligibility::Ready, no_progress_crashes: 0,
            first_run_at: None, terminal_at: None, created_commit: zero, updated_commit: zero,
        };
        let mut batch = WriteBatch::new(); batch.task_upserts.push(task); store.commit(batch).unwrap();
        // Deliberately missing trust object: the real runner fails BEFORE
        // spawning OpenSSH. This tests T1/E1 without any remote/credential I/O.
        let mut missing_trust = input;
        missing_trust.digest = Digest::blake3(b"not sealed: intentionally absent public host trust");
        assert!(!store.cas.object_exists(&missing_trust.digest));
        let binding = SshBinding { target_id: "fixture-node".into(), target: crate::SshTarget::default(), receipt_root: "/unused-fixture".into(), known_hosts: missing_trust };
        Fixture { _root: root, store, node: std::sync::Mutex::new(node), ctx, authority, operation_id, decision_id, target_node, subject, binding }
    }

    fn target_descriptor(f: &Fixture) -> SshTargetReferenceDescriptor {
        let reference = |provider: &str| SshProviderObjectRef {
            provider: provider.into(), object_id: "fixture-only-object".into(), revision: "1".into(),
        };
        SshTargetReferenceDescriptor {
            schema: "ssh.target-references.v1".into(), node_id: f.target_node,
            target_id: "fixture-reference".into(), binding_ref: reference("fixture.binding"),
            permission_ref: reference("fixture.permission"), resource_ref: reference("fixture.resource"),
        }
    }

    fn register_descriptor(f: &Fixture, bytes: &[u8]) {
        f.node.lock().unwrap().handle(Request::Register {
            node_id: f.target_node, target_id: "fixture-reference".into(),
            binding_digest: Digest::blake3(bytes),
        }).unwrap();
    }

    fn requirements(f: &Fixture, bytes: Option<&[u8]>) -> SelectedTargetRequirements {
        SshTaskRunner { store: &f.store }.selected_target_requirements(
            &f.node, &f.ctx, f.operation_id, &f.authority, f.decision_id,
            f.target_node, &f.subject, bytes,
        ).unwrap()
    }

    // Synthetic read-only policy adapter. This does not stand in for a
    // production user policy resolver or establish real target permission.
    enum PolicyMode { Allow, Deny, Fault, Replay, ChangeTask }

    struct TestPolicy<'a> {
        fixture: &'a Fixture,
        id: &'static str,
        mode: std::sync::Mutex<PolicyMode>,
        prior: std::sync::Mutex<Option<Digest>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl<'a> TestPolicy<'a> {
        fn new(fixture: &'a Fixture) -> Self {
            Self {
                fixture, id: "fixture.permission", mode: std::sync::Mutex::new(PolicyMode::Allow),
                prior: std::sync::Mutex::new(None), calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::SshPermissionProvider for TestPolicy<'_> {
        fn provider_id(&self) -> &str { self.id }
        fn implementation_digest(&self) -> Digest { Digest::blake3(b"synthetic-read-only-policy") }
        async fn inspect(&self, request: &crate::SshPermissionRequest<'_>) -> Result<crate::SshPermissionReply> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // No Node lock crosses provider await. Full action is available to
            // policy, but absent from the bridge's Debug/output projection.
            assert!(self.fixture.node.try_lock().is_ok());
            assert!(!format!("{request:?}").contains("synthetic-payload"));
            let scope = request.scope();
            assert_eq!(scope.operation_id, self.fixture.operation_id);
            assert_eq!(scope.decision_id, self.fixture.decision_id);
            assert_eq!(scope.subject, self.fixture.subject);
            assert_eq!(scope.observer_node_id, self.fixture.target_node);
            assert_eq!(scope.descriptor.node_id, self.fixture.target_node);
            assert_eq!(scope.descriptor.permission_ref.revision, "1");
            assert_eq!(scope.logical_key, "fixture-policy");
            assert_eq!(scope.timeout_secs, 1);
            assert_eq!(serde_json::to_value(&scope.invocation)?, serde_json::to_value(&self.fixture.ctx)?);
            assert_eq!(serde_json::to_value(&scope.authority)?, serde_json::to_value(&self.fixture.authority)?);
            assert_eq!(scope.action_digest, Digest::blake3(&serde_json::to_vec(request.action())?));
            assert_eq!(*request.request_digest(), Digest::blake3(&serde_json::to_vec(scope)?));
            assert_eq!(serde_json::to_value(request.action())?["argv"][0], "synthetic-payload");
            let mut digest = request.request_digest().clone();
            let verdict = match *self.mode.lock().unwrap() {
                PolicyMode::Allow => crate::SshPolicyVerdict::Allows,
                PolicyMode::Deny => crate::SshPolicyVerdict::Denied,
                PolicyMode::Fault => anyhow::bail!("private provider diagnostic must not escape"),
                PolicyMode::Replay => {
                    digest = self.prior.lock().unwrap().clone().unwrap();
                    crate::SshPolicyVerdict::Allows
                }
                PolicyMode::ChangeTask => {
                    let mut task = self.fixture.store.external_task(self.fixture.ctx.task_id)?;
                    task.record_revision.0 += 1;
                    let mut batch = WriteBatch::new(); batch.task_upserts.push(task);
                    self.fixture.store.commit(batch)?;
                    crate::SshPolicyVerdict::Allows
                }
            };
            *self.prior.lock().unwrap() = Some(request.request_digest().clone());
            Ok(crate::SshPermissionReply { request_digest: digest, policy_revision: "policy-7".into(), verdict })
        }
    }

    async fn inspect_policy(f: &Fixture, provider: Option<&dyn crate::SshPermissionProvider>) -> crate::SelectedPermissionInspection {
        let bytes = serde_json::to_vec(&target_descriptor(f)).unwrap();
        let action = SshAction::exec(vec!["synthetic-payload".into()], b"synthetic stdin");
        SshTaskRunner { store: &f.store }.inspect_selected_permission(
            &f.node, &f.ctx, f.operation_id, &f.authority, f.decision_id,
            f.target_node, &f.subject, Some(&bytes), "fixture-policy", &action, 1, provider,
        ).await.unwrap()
    }

    fn registered_policy_fixture() -> Fixture {
        claimed_policy_fixture(false)
    }

    // Permission inspection requires a real registered subject head, current
    // dispatcher and Store claim. Keep the older missing-trust SSH fixture
    // unchanged; do not make production fencing tolerate its absent head.
    fn claimed_policy_fixture(with_parent: bool) -> Fixture {
        use shenko_types::subject::*;
        let mut f = fixture();
        let mut task = f.store.external_task(f.ctx.task_id).unwrap();
        let zero = CommitRef::new(f.store.ledger.ledger_id(), LedgerSeq::ZERO);
        let genesis_id = EntityId::new();
        let branch_id = BranchId(f.subject.branch_id.0);
        let head = SubjectBranchHead {
            agent_id: f.subject.agent_id, branch_id: branch_id.clone(),
            subject_root_task_id: EntityId::new(), controller_epoch: f.ctx.epoch,
            software_generation: f.ctx.software_generation.clone(),
            genesis_ref: GenesisRef { ledger_id: f.store.ledger.ledger_id(), genesis_id },
            committed_step_no: 0, committed_step_id: None, committed_at: zero,
            next_step_id: EntityId::new(), mode: SubjectMode::Active,
            mode_reason_ref: None, record_revision: RecordRevision(0),
        };
        let genesis = SubjectGenesis {
            genesis_id, subject_id: f.subject.agent_id,
            bootstrap_id: EntityId::new(), launch_id: EntityId::new(),
            seed_manifest_ref: task.input_ref.clone(), initial_profile_ref: task.input_ref.clone(),
            home_manifest_ref: task.input_ref.clone(), bootstrap_event_ref: task.input_ref.clone(),
            created_commit: zero,
        };
        let step = SubjectStep {
            step_id: head.next_step_id, branch_id, step_no: 1,
            expected_parent_commit: zero, controller_epoch: head.controller_epoch,
            software_generation: head.software_generation.clone(), state: SubjectStepState::Planned,
            input_plan_ref: None, context_revision: None, active_request_id: None,
            active_attempt_id: None, model_result_ref: None, model_outcome_ref: None,
            decision_ref: None, commit_ref: None, retry_reason_ref: None,
            record_revision: RecordRevision(0), created_by_step_id: None,
        };
        f.store.initialize_genesis(&genesis, &head, &step).unwrap();
        f.store.start_subject_dispatcher().unwrap();
        task.state = TaskState::Pending { checkpoint_ref: None };
        task.memos.remove(NATIVE_ASSIGNMENT);
        task.memos.insert(shenko_store::child::NATIVE_INPUT.into(), serde_json::json!({"fixture": true}));
        let implementation = task.implementation_ref.clone();
        let mut batch = WriteBatch::new();
        if with_parent {
            let mut parent = task.clone();
            parent.task_id = EntityId::new(); parent.memos.clear();
            task.owner = TaskOwner::Task { id: parent.task_id };
            batch.task_upserts.push(parent);
        }
        batch.task_upserts.push(task);
        f.store.commit(batch).unwrap();
        let claim = f.store.claim_native_task(f.ctx.task_id, implementation, false).unwrap().unwrap();
        f.ctx = claim.invocation;
        assert_eq!(claim.operation_id, f.operation_id);
        assert!(f.store.native_invocation_current(&f.ctx).unwrap());
        register_descriptor(&f, &serde_json::to_vec(&target_descriptor(&f)).unwrap());
        f
    }

    // A real suspension point lets a concurrent test task change durable
    // state before the synthetic provider returns its otherwise valid reply.
    struct WaitingPolicy<'a> {
        inner: TestPolicy<'a>,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl crate::SshPermissionProvider for WaitingPolicy<'_> {
        fn provider_id(&self) -> &str { self.inner.id }
        fn implementation_digest(&self) -> Digest {
            crate::SshPermissionProvider::implementation_digest(&self.inner)
        }
        async fn inspect(&self, request: &crate::SshPermissionRequest<'_>) -> Result<crate::SshPermissionReply> {
            self.entered.notify_one();
            self.release.notified().await;
            crate::SshPermissionProvider::inspect(&self.inner, request).await
        }
    }

    async fn inspect_while(f: &Fixture, change: impl FnOnce()) -> crate::SelectedPermissionInspection {
        let provider = WaitingPolicy {
            inner: TestPolicy::new(f), entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        };
        let mutation = async {
            provider.entered.notified().await;
            change();
            provider.release.notify_one();
        };
        // If the provider was never entered, this fails rather than hanging.
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(inspect_policy(f, Some(&provider)), mutation)
        }).await.expect("policy callback did not reach its test suspension");
        assert_eq!(provider.inner.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(f.store.external_operation(f.operation_id).is_err());
        assert!(f.store.external_operation_evidence(f.operation_id).unwrap().is_empty());
        result
    }

    fn inject_generation_change(f: &Fixture) {
        // Only generation has no public mutation API in this source. This
        // explicit SQLite fault injection is NOT a production upgrade path.
        let (mut head, mut step) = f.store.load_subject(f.subject.agent_id).unwrap();
        head.software_generation = RuntimeGeneration::new("replaced-during-policy");
        step.software_generation = head.software_generation.clone();
        let mut conn = rusqlite::Connection::open(f._root.join("store.sqlite3")).unwrap();
        let tx = conn.transaction().unwrap();
        assert_eq!(tx.execute(
            "UPDATE subject_branch_heads SET data=?1 WHERE agent_id=?2",
            [serde_json::to_string(&head).unwrap(), f.subject.agent_id.to_string()],
        ).unwrap(), 1);
        assert_eq!(tx.execute(
            "UPDATE subject_steps SET data=?1 WHERE step_id=?2",
            [serde_json::to_string(&step).unwrap(), step.step_id.to_string()],
        ).unwrap(), 1);
        tx.commit().unwrap();
        assert_eq!(f.store.load_subject(f.subject.agent_id).unwrap().0.software_generation,
            RuntimeGeneration::new("replaced-during-policy"));
    }

    #[tokio::test]
    async fn permission_bridge_rejects_missing_runtime_state_before_callback() {
        use crate::{PermissionInspectionUnavailable as U, SelectedPermissionInspection as I};
        let f = fixture();
        f.store.start_subject_dispatcher().unwrap(); // Head is deliberately absent.
        register_descriptor(&f, &serde_json::to_vec(&target_descriptor(&f)).unwrap());
        let provider = TestPolicy::new(&f);
        assert!(matches!(inspect_policy(&f, Some(&provider)).await,
            I::Unavailable { reason: U::InvocationCheckUnavailable }));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(f.store.external_operation(f.operation_id).is_err());
    }

    #[tokio::test]
    async fn permission_bridge_fences_stale_generation_before_callback() {
        use crate::{PermissionInspectionUnavailable as U, SelectedPermissionInspection as I};
        let f = registered_policy_fixture(); let provider = TestPolicy::new(&f);
        inject_generation_change(&f);
        assert!(matches!(inspect_policy(&f, Some(&provider)).await,
            I::Unavailable { reason: U::InvocationNotCurrent }));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(f.store.external_operation(f.operation_id).is_err());
    }

    #[tokio::test]
    async fn permission_bridge_fences_generation_changed_while_awaiting() {
        use crate::{PermissionInspectionUnavailable as U, SelectedPermissionInspection as I};
        let f = registered_policy_fixture();
        assert!(matches!(inspect_while(&f, || inject_generation_change(&f)).await,
            I::Unavailable { reason: U::InvocationNotCurrent }));
    }

    #[tokio::test]
    async fn permission_bridge_fences_cancelled_ancestor_while_awaiting() {
        use crate::{PermissionInspectionUnavailable as U, SelectedPermissionInspection as I};
        let f = claimed_policy_fixture(true);
        let before = f.store.external_task(f.ctx.task_id).unwrap();
        let TaskOwner::Task { id } = before.owner else { panic!("fixture parent missing") };
        let result = inspect_while(&f, || {
            let cancelled = f.store.request_native_cancellation(
                id, EntityId::new(), EntityId::new(), "fixture ancestor revoked", AbortScope::Task,
            ).unwrap();
            assert_eq!(cancelled.cancelled, vec![id]);
            // Ancestor-only cancellation: child metadata alone cannot detect it.
            let after = f.store.external_task(f.ctx.task_id).unwrap();
            assert_eq!(after.record_revision, before.record_revision);
            assert!(after.abort_request.is_none());
        }).await;
        assert!(matches!(result, I::Unavailable { reason: U::InvocationCheckUnavailable }));
    }

    #[tokio::test]
    async fn permission_bridge_fences_task_revoked_while_awaiting() {
        use crate::{PermissionInspectionUnavailable as U, SelectedPermissionInspection as I};
        let f = registered_policy_fixture();
        let result = inspect_while(&f, || {
            let cancelled = f.store.request_native_cancellation(
                f.ctx.task_id, EntityId::new(), EntityId::new(), "fixture task revoked", AbortScope::Task,
            ).unwrap();
            assert_eq!(cancelled.cancelled, vec![f.ctx.task_id]);
        }).await;
        assert!(matches!(result, I::Unavailable { reason: U::ReferencesChanged }));
    }

    #[tokio::test]
    async fn permission_bridge_fences_dispatcher_takeover_while_awaiting() {
        use crate::{PermissionInspectionUnavailable as U, SelectedPermissionInspection as I};
        let f = registered_policy_fixture();
        let result = inspect_while(&f, || {
            let successor = Store::open(f._root.join("store.sqlite3"), f._root.join("cas")).unwrap();
            successor.start_subject_dispatcher().unwrap();
        }).await;
        assert!(matches!(result, I::Unavailable { reason: U::InvocationCheckUnavailable }));
    }

    #[tokio::test]
    async fn permission_bridge_requires_exact_installed_provider_before_disclosing_action() {
        use crate::{PermissionInspectionUnavailable as U, SelectedPermissionInspection as I};
        let f = registered_policy_fixture();
        assert!(matches!(inspect_policy(&f, None).await, I::Unavailable { reason: U::ProviderNotInstalled }));
        let mut provider = TestPolicy::new(&f); provider.id = "unrelated.policy";
        assert!(matches!(inspect_policy(&f, Some(&provider)).await, I::Unavailable { reason: U::ProviderIdentityMismatch }));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(f.store.external_operation(f.operation_id).is_err());
    }

    #[tokio::test]
    async fn permission_bridge_observes_exact_scope_without_grant_and_rechecks_revocation() {
        use crate::{PermissionInspectionUnavailable as U, SelectedPermissionInspection as I};
        let f = registered_policy_fixture(); let provider = TestPolicy::new(&f);
        let observed = inspect_policy(&f, Some(&provider)).await;
        let json = serde_json::to_value(&observed).unwrap();
        assert_eq!(json["status"], "policy_observed");
        assert_eq!(json["execution_authority"], "not_granted");
        assert_eq!(json["dispatch"], "not_dispatched");
        assert!(!json.to_string().contains("synthetic-payload"));
        assert!(f.store.external_operation(f.operation_id).is_err());
        assert!(f.store.external_operation_evidence(f.operation_id).unwrap().is_empty());
        *provider.mode.lock().unwrap() = PolicyMode::Deny;
        assert!(matches!(inspect_policy(&f, Some(&provider)).await, I::Unavailable { reason: U::PolicyDenied }));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn permission_bridge_rejects_prior_reply_for_identical_request() {
        use crate::{PermissionInspectionUnavailable as U, SelectedPermissionInspection as I};
        let f = registered_policy_fixture(); let provider = TestPolicy::new(&f);
        assert!(matches!(inspect_policy(&f, Some(&provider)).await, I::PolicyObserved { .. }));
        *provider.mode.lock().unwrap() = PolicyMode::Replay;
        assert!(matches!(inspect_policy(&f, Some(&provider)).await, I::Unavailable { reason: U::ProviderReplyMismatch }));
        assert!(f.store.external_operation(f.operation_id).is_err());
    }

    #[tokio::test]
    async fn permission_bridge_rechecks_task_after_provider_await() {
        use crate::{PermissionInspectionUnavailable as U, SelectedPermissionInspection as I};
        let f = registered_policy_fixture(); let provider = TestPolicy::new(&f);
        *provider.mode.lock().unwrap() = PolicyMode::ChangeTask;
        assert!(matches!(inspect_policy(&f, Some(&provider)).await, I::Unavailable { reason: U::ReferencesChanged }));
        assert!(f.store.external_operation(f.operation_id).is_err());
    }

    #[tokio::test]
    async fn permission_bridge_sanitizes_provider_failure() {
        use crate::{PermissionInspectionUnavailable as U, SelectedPermissionInspection as I};
        let f = registered_policy_fixture(); let provider = TestPolicy::new(&f);
        *provider.mode.lock().unwrap() = PolicyMode::Fault;
        let result = inspect_policy(&f, Some(&provider)).await;
        assert!(matches!(&result, I::Unavailable { reason: U::ProviderFailed }));
        assert!(!serde_json::to_string(&result).unwrap().contains("private provider"));
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(f.store.external_operation(f.operation_id).is_err());
    }

    #[test]
    fn registered_references_remain_unresolved_not_authority_or_admission() {
        let f = fixture();
        assert!(matches!(requirements(&f, None), SelectedTargetRequirements::Unavailable {
            reason: TargetReferenceUnavailable::TargetNotRegistered,
        }));
        let descriptor = target_descriptor(&f);
        let bytes = serde_json::to_vec(&descriptor).unwrap();
        register_descriptor(&f, &bytes);
        assert!(matches!(requirements(&f, None), SelectedTargetRequirements::Unavailable {
            reason: TargetReferenceUnavailable::DescriptorUnavailable,
        }));
        let result = requirements(&f, Some(&bytes));
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["status"], "requires_providers");
        assert_eq!(json["task_id"], serde_json::json!(f.ctx.task_id));
        assert_eq!(json["operation_id"], serde_json::json!(f.operation_id));
        assert_eq!(json["decision_id"], serde_json::json!(f.decision_id));
        assert_eq!(json["descriptor"]["permission_ref"]["provider"], "fixture.permission");
        assert_eq!(json["descriptor"]["resource_ref"]["provider"], "fixture.resource");
        assert_eq!(json["execution_authority"], "not_granted");
        assert_eq!(json["dispatch"], "not_dispatched");
        assert!(f.store.external_operation(f.operation_id).is_err());
        assert!(f.store.external_operation_evidence(f.operation_id).unwrap().is_empty());
    }

    #[test]
    fn descriptor_cannot_substitute_target_or_smuggle_credential_fields() {
        let f = fixture();
        let descriptor = target_descriptor(&f);
        let mut value = serde_json::to_value(descriptor).unwrap();
        value["key_path"] = serde_json::json!("/not-a-real-key");
        let bytes = serde_json::to_vec(&value).unwrap();
        register_descriptor(&f, &bytes);
        assert!(matches!(requirements(&f, Some(&bytes)), SelectedTargetRequirements::Unavailable {
            reason: TargetReferenceUnavailable::DescriptorInvalid,
        }));
        value.as_object_mut().unwrap().remove("key_path");
        value["node_id"] = serde_json::json!(EntityId::new());
        let altered = serde_json::to_vec(&value).unwrap();
        assert!(matches!(requirements(&f, Some(&altered)), SelectedTargetRequirements::Unavailable {
            reason: TargetReferenceUnavailable::DescriptorDigestMismatch,
        }));
        assert!(f.store.external_operation(f.operation_id).is_err());
    }

    #[test]
    fn reference_inspection_rejects_stale_invocation_and_foreign_subject() {
        let f = fixture();
        let runner = SshTaskRunner { store: &f.store };
        let mut wrong = f.subject.clone(); wrong.branch_id = EntityId::new();
        assert!(matches!(runner.selected_target_requirements(
            &f.node, &f.ctx, f.operation_id, &f.authority, f.decision_id, f.target_node, &wrong, None,
        ).unwrap(), SelectedTargetRequirements::Unavailable {
            reason: TargetReferenceUnavailable::SelectionReferenceUnavailable,
        }));
        let mut stale = f.ctx.clone(); stale.worker_id = EntityId::new();
        assert!(matches!(runner.selected_target_requirements(
            &f.node, &stale, f.operation_id, &f.authority, f.decision_id, f.target_node, &f.subject, None,
        ).unwrap(), SelectedTargetRequirements::Unavailable {
            reason: TargetReferenceUnavailable::NativeInvocation(SelectedSshUnavailable::AssignmentChanged),
        }));
        assert!(f.store.external_operation(f.operation_id).is_err());
    }

    #[tokio::test]
    async fn armed_ssh_target_never_reresolves_current_registry_references() {
        let f = fixture();
        let first = SshTaskRunner { store: &f.store }.execute_selected(
            &f.node, &f.ctx, f.operation_id, "fixture", f.authority.clone(),
            f.decision_id, f.target_node, f.binding.clone(),
            SshAction::exec(vec!["must-not-spawn".into()], b""), 1,
        ).await.unwrap();
        assert!(matches!(first, SelectedSshOutcome::Observed(_)));
        let before = f.store.external_operation_attempts(f.operation_id).unwrap();
        // No registration/descriptor exists. Already-armed identity wins over
        // current resolution; only the original SSH query reader may recover.
        assert!(matches!(requirements(&f, None), SelectedTargetRequirements::Unavailable {
            reason: TargetReferenceUnavailable::OperationAlreadyArmed,
        }));
        let after = f.store.external_operation_attempts(f.operation_id).unwrap();
        assert_eq!(before.len(), 1); assert_eq!(after.len(), 1);
        assert_eq!(before[0].attempt_id, after[0].attempt_id);
    }

    #[tokio::test]
    async fn selection_is_sealed_in_real_t1_and_recovery_never_reselects() {
        let mut f = fixture();
        let action = SshAction::exec(vec!["must-not-spawn".into()], b"");
        let first = SshTaskRunner { store: &f.store }.execute_selected(&mut f.node, &f.ctx, f.operation_id, "fixture", f.authority.clone(), f.decision_id, f.target_node, f.binding.clone(), action.clone(), 1).await.unwrap();
        let SelectedSshOutcome::Observed(observation) = first else { panic!("expected a real T1/E1 observation") };
        assert!(observation.transport.is_none());
        assert!(observation.diagnostic.is_some());
        let op = f.store.external_operation(f.operation_id).unwrap();
        assert_eq!(op.effect_status, shenko_types::operation::EffectStatus::Unknown);
        let bytes = f.store.cas.read_object(&op.final_input_ref.digest).unwrap();
        let frozen: FrozenInput = serde_json::from_slice(&bytes).unwrap();
        let bound = frozen.selection.as_ref().unwrap();
        assert_eq!(serde_json::to_value(&bound.invocation).unwrap(), serde_json::to_value(&f.ctx).unwrap());
        assert_eq!(bound.receipt.selection.decision_id, f.decision_id);
        assert_eq!(bound.receipt.selection.reason, "explicit fixture choice, not a permission grant");
        // A changed Node database cannot rewrite the already-armed operation.
        let other = f._root.join("different-node");
        let mut other_node = Node::open(&other).unwrap();
        other_node.handle(Request::Initialize { node_id: EntityId::new(), identity: None }).unwrap();
        f.node = std::sync::Mutex::new(other_node);
        let second = SshTaskRunner { store: &f.store }.execute_selected(&mut f.node, &f.ctx, f.operation_id, "fixture", f.authority.clone(), f.decision_id, f.target_node, f.binding.clone(), action, 1).await.unwrap();
        assert!(matches!(second, SelectedSshOutcome::Observed(_)));
        assert_eq!(f.store.external_operation_attempts(f.operation_id).unwrap().len(), 1);
        assert_eq!(f.store.external_operation(f.operation_id).unwrap().input_digest, op.input_digest);
    }

    #[tokio::test]
    async fn forged_worker_is_refused_before_t1() {
        let mut f = fixture(); f.ctx.worker_id = EntityId::new();
        let result = SshTaskRunner { store: &f.store }.execute_selected(&mut f.node, &f.ctx, f.operation_id, "fixture", f.authority, f.decision_id, f.target_node, f.binding, SshAction::exec(vec!["must-not-spawn".into()], b""), 1).await.unwrap();
        assert!(matches!(result, SelectedSshOutcome::Unavailable(SelectedSshUnavailable::AssignmentChanged)));
        assert!(f.store.external_operation(f.operation_id).is_err());
    }

    #[tokio::test]
    async fn unrelated_authority_is_refused_before_t1() {
        let mut f = fixture(); f.authority.decision_ref = EntityId::new();
        let result = SshTaskRunner { store: &f.store }.execute_selected(&mut f.node, &f.ctx, f.operation_id, "fixture", f.authority, f.decision_id, f.target_node, f.binding, SshAction::exec(vec!["must-not-spawn".into()], b""), 1).await.unwrap();
        assert!(matches!(result, SelectedSshOutcome::Unavailable(SelectedSshUnavailable::AuthorityMismatch)));
        assert!(f.store.external_operation(f.operation_id).is_err());
    }

    #[tokio::test]
    async fn different_registered_target_is_not_a_fallback() {
        let mut f = fixture();
        let result = SshTaskRunner { store: &f.store }.execute_selected(&mut f.node, &f.ctx, f.operation_id, "fixture", f.authority, f.decision_id, EntityId::new(), f.binding, SshAction::exec(vec!["must-not-spawn".into()], b""), 1).await.unwrap();
        assert!(matches!(result, SelectedSshOutcome::Unavailable(SelectedSshUnavailable::TargetBindingMismatch)));
        assert!(f.store.external_operation(f.operation_id).is_err());
    }

    #[tokio::test]
    async fn conflicting_retry_cannot_change_action_or_dispatch_twice() {
        let mut f = fixture();
        let runner = SshTaskRunner { store: &f.store };
        runner.execute_selected(&mut f.node, &f.ctx, f.operation_id, "fixture", f.authority.clone(), f.decision_id, f.target_node, f.binding.clone(), SshAction::exec(vec!["original".into()], b""), 1).await.unwrap();
        let result = runner.execute_selected(&mut f.node, &f.ctx, f.operation_id, "fixture", f.authority, f.decision_id, f.target_node, f.binding, SshAction::exec(vec!["changed".into()], b""), 1).await.unwrap();
        assert!(matches!(result, SelectedSshOutcome::Unavailable(SelectedSshUnavailable::FrozenSelectionConflict)));
        assert_eq!(f.store.external_operation_attempts(f.operation_id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn changed_runtime_generation_cannot_reuse_selected_invocation() {
        let f = fixture();
        let runner = SshTaskRunner { store: &f.store };
        let action = SshAction::exec(vec!["original".into()], b"");
        runner.execute_selected(&f.node, &f.ctx, f.operation_id, "fixture", f.authority.clone(), f.decision_id, f.target_node, f.binding.clone(), action.clone(), 1).await.unwrap();
        let original = f.store.external_operation(f.operation_id).unwrap();
        let mut changed = f.ctx.clone();
        changed.software_generation = RuntimeGeneration::new("replacement-generation");
        let result = runner.execute_selected(&f.node, &changed, f.operation_id, "fixture", f.authority, f.decision_id, f.target_node, f.binding, action, 1).await.unwrap();
        assert!(matches!(result, SelectedSshOutcome::Unavailable(SelectedSshUnavailable::FrozenSelectionConflict)));
        let retained = f.store.external_operation(f.operation_id).unwrap();
        assert_eq!(retained.input_digest, original.input_digest);
        assert_eq!(retained.evidence_through, original.evidence_through);
        assert_eq!(f.store.external_operation_attempts(f.operation_id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn legacy_selected_input_remains_query_only_without_inventing_generation() {
        let f = fixture();
        let runner = SshTaskRunner { store: &f.store };
        let action = SshAction::exec(vec!["legacy".into()], b"");
        let receipt = f.node.lock().unwrap().node_selection_receipt(f.decision_id).unwrap().unwrap();
        let mut old_invocation = serde_json::to_value(&f.ctx).unwrap();
        old_invocation.as_object_mut().unwrap().remove("software_generation");
        // Exact v1 shape, synthetically projected from this fixture. This is
        // not an old executable or real remote migration success claim.
        let old = serde_json::json!({
            "binding": &f.binding, "action": &action,
            "selection": {
                "schema": "ssh-task-node-selection.v1", "invocation": old_invocation,
                "authority": &f.authority, "operation_id": f.operation_id,
                "registered_node_id": f.target_node, "receipt": receipt,
            },
        });
        let frozen: FrozenInput = serde_json::from_value(old.clone()).unwrap();
        frozen.selection.as_ref().unwrap().ensure_readable().unwrap();
        assert_eq!(serde_json::to_value(&frozen).unwrap(), old);
        // Even the crate-private admission helper refuses a fresh legacy input.
        // Only the reader above and the existing operation query may accept it.
        let error = runner.execute_frozen(&f.ctx, f.operation_id, "fixture", f.authority, frozen, 1).await.unwrap_err();
        assert!(error.to_string().contains("query-only"));
        assert!(f.store.external_operation(f.operation_id).is_err());
        // A v2 record may not erase its generation and fall through to v1.
        let mut invalid = old.clone();
        invalid["selection"]["schema"] = "ssh-task-node-selection.v2".into();
        let invalid: FrozenInput = serde_json::from_value(invalid).unwrap();
        assert!(invalid.selection.unwrap().ensure_readable().is_err());
        // Nor may v1 carry a generation, even one matching the current caller.
        let mut invalid = old.clone();
        invalid["selection"]["invocation"]["software_generation"] = serde_json::to_value(&f.ctx.software_generation).unwrap();
        let invalid: FrozenInput = serde_json::from_value(invalid).unwrap();
        assert!(invalid.selection.unwrap().ensure_readable().is_err());
        // Unknown legacy fields and wrongly typed generations cannot downgrade.
        let mut invalid = old.clone();
        invalid["selection"]["invocation"]["unrecognized_authority"] = true.into();
        assert!(serde_json::from_value::<FrozenInput>(invalid).is_err());
        let mut invalid = old;
        invalid["selection"]["invocation"]["software_generation"] = 7.into();
        assert!(serde_json::from_value::<FrozenInput>(invalid).is_err());
    }

    #[test]
    fn legacy_input_bytes_are_unchanged_when_selection_is_absent() {
        let f = fixture();
        let action = SshAction::exec(vec!["legacy".into()], b"");
        #[derive(Serialize)] struct Legacy<'a> { binding: &'a SshBinding, action: &'a SshAction }
        let old = serde_json::to_vec(&Legacy { binding: &f.binding, action: &action }).unwrap();
        let frozen = FrozenInput { binding: f.binding, action, selection: None };
        assert_eq!(serde_json::to_vec(&frozen).unwrap(), old);
        assert!(serde_json::from_slice::<FrozenInput>(&old).unwrap().selection.is_none());
    }
}
