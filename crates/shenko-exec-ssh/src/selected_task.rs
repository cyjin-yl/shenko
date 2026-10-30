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

fn check_task(
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
    use shenko_node::{NodeSelection, Request};
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
        binding: SshBinding,
    }

    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!("shenko-selected-ssh-{}", EntityId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::open(root.join("store.sqlite3"), root.join("cas")).unwrap();
        let mut node = Node::open(&root.join("node")).unwrap();
        let target_node = EntityId::new();
        node.handle(Request::Initialize { node_id: target_node, identity: None }).unwrap();
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
            record_revision: ctx.record_revision, agent_id: EntityId::new(), conversation_id,
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
        Fixture { _root: root, store, node: std::sync::Mutex::new(node), ctx, authority, operation_id, decision_id, target_node, binding }
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
