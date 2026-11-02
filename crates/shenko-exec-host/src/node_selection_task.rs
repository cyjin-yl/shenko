//! Installed explicit node choice, not target authorization or dispatch.
//! The native registry supplies task identity; the operation ID names the
//! decision. Only fresh T1 may write the existing Node command journal.
//! Recovery queries that exact receipt and never replays an uncertain write.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_node::{Identity, Node, NodeSelection, NodeSelectionOutcome, NodeSelectionReceipt};
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt},
    ids::*,
    object::*,
    operation::*,
    task::{ImplementationRef, TaskRecord},
};
use std::sync::{Arc, Mutex};

const CONTRACT: &[u8] = b"shenko.node.selection.record/v1: explicit node/digest/reason recorded in the bound subject installation command journal under the trusted operation ID; recovery queries only the original receipt, never records again; no target authorization or dispatch";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionInput {
    node_id: EntityId,
    observed_profile_digest: Digest,
    reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenInput {
    schema_version: u32,
    observer_node_id: EntityId,
    subject: Identity,
    selection: NodeSelection,
}

/// One connection to the Node already opened and identity-checked at startup.
/// This is not a second registry or an administrative Node RPC adapter.
pub struct NodeSelectionHandler {
    store: Arc<Store>,
    node: Mutex<Node>,
}

impl NodeSelectionHandler {
    pub fn new(store: Arc<Store>, node: Node) -> Self {
        Self { store, node: Mutex::new(node) }
    }

    fn plugin_id() -> EntityId {
        "0f5dcadb-8760-4249-9a65-718fbf20e194"
            .parse()
            .expect("fixed node-selection plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            bytes, "application/json", "native-node-selection", schema, 1,
        ) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn freeze(&self, value: serde_json::Value, subject: Identity, decision_id: EntityId) -> Result<FrozenInput> {
        let SelectionInput { node_id, observed_profile_digest, reason } = serde_json::from_value(value)?;
        let selection = NodeSelection { decision_id, node_id, observed_profile_digest, reason };
        selection.validate()?;
        let node = self.node.lock().map_err(|_| anyhow::anyhow!("node_recorder_poisoned"))?;
        ensure!(node.local_identity()?.as_ref() == Some(&subject), "node_subject_binding_stale");
        Ok(FrozenInput {
            schema_version: 1,
            observer_node_id: node.local_node_id()?.context("node_not_initialized")?,
            subject,
            selection,
        })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "node_selection_frozen_input_mismatch"
        );
        let frozen: FrozenInput = serde_json::from_slice(&bytes)?;
        ensure!(frozen.schema_version == 1, "unsupported_node_selection_input_schema");
        frozen.selection.validate()?;
        ensure!(frozen.selection.decision_id == operation.operation_id, "frozen_decision_id_mismatch");
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        frozen: &FrozenInput,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "node.selection_journal.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "local_selection_receipt_only".into(),
            extra: serde_json::json!({"observer_node_id": frozen.observer_node_id, "subject": frozen.subject}),
        };
        let resource = self.seal(&bindings, "node-selection-record-binding")?;
        let acceptance = self.seal(
            &"one explicit decision receipt in the local Node command journal; complete means only this local record, not target readiness, authorization or dispatch",
            "node-selection-record-acceptance",
        )?;
        let retry = self.seal(
            &"query only the exact original decision receipt under its frozen installation/subject; missing or unreadable remains unknown, never write, choose again, refresh observations or arm again",
            "node-selection-record-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:node-selection-record-v1".into(),
            },
            symbol: "node.selection.record.execute".into(),
            schema: SchemaRef {
                name: "node-selection-record-observation-v1".into(),
                major: 1,
                digest: contract_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: "node.selection.record.v1".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            input_digest: input_ref.digest.clone(),
            final_input_ref: input_ref,
            contract_ref: OperationContractRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: contract_digest.clone(),
                contract_digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::None,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: invocation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        Ok((operation, attempt))
    }

    fn record(&self, frozen: &FrozenInput) -> Result<NodeSelectionReceipt> {
        let mut node = self.node.lock().map_err(|_| anyhow::anyhow!("node_recorder_poisoned"))?;
        node.record_node_selection_for_subject(
            frozen.selection.clone(), frozen.observer_node_id, &frozen.subject,
        )
    }

    fn receipt(&self, frozen: &FrozenInput) -> Result<Option<NodeSelectionReceipt>> {
        let mut node = self.node.lock().map_err(|_| anyhow::anyhow!("node_recorder_poisoned"))?;
        let receipt = node.node_selection_receipt_for_subject(
            frozen.selection.decision_id, frozen.observer_node_id, &frozen.subject,
        )?;
        if let Some(receipt) = &receipt {
            ensure!(
                serde_json::to_value(&receipt.selection)? == serde_json::to_value(&frozen.selection)?,
                "selection_receipt_frozen_input_mismatch"
            );
        }
        Ok(receipt)
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenInput,
        receipt: Option<NodeSelectionReceipt>,
    ) -> Result<OperationRecord> {
        // Store and Node are distinct databases. A crash/write error after T1
        // does not prove that the Node write did not occur. Repeated absence
        // must not grow identical unknown evidence or authorize another write.
        if receipt.is_none() && operation.state == OperationState::Reconciling
            && operation.effect_status == EffectStatus::Unknown
        {
            return Ok(operation.clone());
        }
        let (result, outcome, effect) = match receipt {
            Some(receipt) => {
                let outcome = match &receipt.outcome {
                    NodeSelectionOutcome::Recorded => OperationOutcome::Succeeded,
                    NodeSelectionOutcome::Unavailable { .. } => OperationOutcome::Failed,
                };
                (serde_json::json!({
                    "schema_version": 1,
                    "status": "receipt",
                    "observer_node_id": frozen.observer_node_id,
                    "receipt": receipt,
                    "effect_scope": "local_selection_journal_only",
                    "execution_authority": "not_granted",
                    "dispatch": "not_dispatched",
                }), Some(outcome), EffectStatus::Complete)
            }
            None => (serde_json::json!({
                "schema_version": 1,
                "status": "unknown",
                "error": {"code": "original_selection_receipt_unavailable", "message": "the original receipt is absent or unreadable under its frozen binding; no replacement decision or automatic replay"},
                "decision_id": frozen.selection.decision_id,
                "effect_scope": "local_selection_journal_only",
                "execution_authority": "not_granted",
                "dispatch": "not_dispatched",
            }), None, EffectStatus::Unknown),
        };
        // Database paths and underlying provider errors are never projected.
        let sealed = self.seal(&result, "node-selection-record-observation")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "node.selection.record.v1".into(),
            source_instance_id: frozen.observer_node_id.to_string(),
            source_event_key: Some(format!("node-selection:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: sealed.clone(),
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(sealed),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id, attempt.attempt_id, evidence, outcome, effect,
        )
    }

    fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "node.selection.record" && invocation.task.definition_version == 1,
            "node_selection_task_identity_mismatch"
        );
        let claim = &invocation.task_invocation;
        let step_id = invocation.task.created_by_step_id.context("node_selection_has_no_subject_step")?;
        // Defensive wiring checks, not authentication of arbitrary Rust
        // callers. The installed native registry owns the claim/authority.
        ensure!(
            invocation.task.task_id == claim.task_id
                && invocation.task.task_revision == claim.task_revision
                && invocation.context.task_id == Some(claim.task_id)
                && invocation.context.expected_task_revision == Some(claim.task_revision.0)
                && invocation.context.invocation_id == Some(claim.invocation_id)
                && invocation.context.controller_epoch == claim.epoch
                && invocation.context.software_generation == claim.software_generation
                && invocation.authority.delegation_id == claim.task_id
                && invocation.authority.delegation_revision.0 == claim.task_revision.0
                && invocation.authority.decision_ref == step_id,
            "node_selection_trusted_invocation_mismatch"
        );
        let (head, _) = self.store.load_subject(invocation.task.agent_id)?;
        self.store.get_step(&head.branch_id, step_id)?.context("node_selection_origin_outside_subject_branch")?;
        let subject = {
            let node = self.node.lock().map_err(|_| anyhow::anyhow!("node_recorder_poisoned"))?;
            let identity = node.local_identity()?.context("node_has_no_subject_identity")?;
            ensure!(
                identity.agent_id == invocation.task.agent_id && identity.branch_id == EntityId(head.branch_id.0),
                "node_subject_binding_stale"
            );
            identity
        };
        let frozen = self.freeze(std::mem::take(&mut invocation.input), subject, invocation.operation_id)?;
        let input_ref = self.seal(&frozen, "node-selection-record-frozen-input")?;
        let (mut operation, mut attempt) = self.operation_parts(&invocation, &frozen, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation, &mut operation, &mut attempt,
        )? {
            return Ok(existing);
        }
        // This is the only write site. Even a matching repeated arm above
        // returns the original operation and never reaches it again.
        let receipt = self.record(&frozen).ok();
        self.observe(&operation, &attempt, &frozen, receipt)
    }
}

#[async_trait]
impl NativeTaskHandler for NodeSelectionHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "node.selection.record".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-node-selection-recorder/1".into(),
            authority_refs: vec!["node.selection.record.own_installation/v1".into()],
            prompt_section: Some(
                "node.selection.record/v1 是已安装的显式节点选择记录 child。先依据实际观测自行判断，再用 [delegate:node.selection.record:1] {\"node_id\":\"节点 UUID\",\"observed_profile_digest\":\"观测返回的完整 blake3 摘要\",\"reason\":\"选择理由\"} 记录这一选择；reason 为 1..4096 UTF-8 字节。不要自行计算或猜测摘要，不传 decision_id、路径、权限或 SSH 命令。decision_id 由真实操作生成。receipt 的 recorded 仅说明选择已写入本地 journal，不是节点仍可用、资源预约、权限授予或执行完成；unavailable 返回当时不匹配的事实，由主体决定是否重新观察或另作选择，不自动换节点。恢复只查询原回执；unknown 保持未知，不能据此重复同一效果。此能力不执行 SSH，目标执行仍须独立验证真实 target/resource 权限。"
                .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation)
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == "node.selection.record.v1"
                && task.kind == "node.selection.record" && task.definition_version == 1
                && operation.contract_ref.plugin_id == Self::plugin_id()
                && operation.contract_ref.contract_digest == Digest::blake3(CONTRACT),
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if !matches!(operation.state, OperationState::InFlight | OperationState::Reconciling)
            || !matches!(attempt.dispatch_stage, AttemptDispatchStage::Armed | AttemptDispatchStage::Unknown)
            || operation.outcome_conflict
        {
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        ensure!(frozen.subject.agent_id == task.agent_id, "original_selection_subject_mismatch");
        // Never call record here: after T1 we cannot know whether the original
        // writer is delayed, crashed, or committed before E1. Query-only
        // reconciliation may resolve a later visible receipt under that same
        // operation/attempt, without reconsidering current advertisements.
        let receipt = self.receipt(&frozen).ok().flatten();
        self.observe(&operation, &attempt, &frozen, receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_node::{Request, CapabilityKnowledge, SelectionUnavailable, RoleAssignmentRequest, NodeRole};
    use shenko_store::TaskInvocation;
    use shenko_types::agent::CallContext;
    use shenko_types::store::WriteBatch;
    use shenko_types::task::{DispatchEligibility, RunMode, TaskOwner, TaskState};
    use std::path::{Path, PathBuf};

    fn identity() -> Identity {
        Identity {
            agent_id: EntityId::new(), branch_id: EntityId::new(),
            origin_proof: Digest::blake3(b"node-selection-fixture-origin"),
        }
    }

    fn node(path: &Path, subject: &Identity) -> Node {
        let mut node = Node::open(path).unwrap();
        node.handle(Request::Initialize {
            node_id: EntityId::new(), identity: Some(subject.clone()),
        }).unwrap();
        node
    }

    fn fixture() -> (PathBuf, Arc<Store>, NodeSelectionHandler, Identity) {
        let dir = std::env::temp_dir().join(format!("shenko-node-selection-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
        let subject = identity();
        let handler = NodeSelectionHandler::new(store.clone(), node(&dir.join("node"), &subject));
        (dir, store, handler, subject)
    }

    fn choice(handler: &NodeSelectionHandler) -> serde_json::Value {
        let mut node = handler.node.lock().unwrap();
        let profile = node.capability_profile().unwrap();
        let candidate = node.node_choice_candidate(profile.node_id).unwrap().unwrap();
        let CapabilityKnowledge::Observed { observation } = candidate.knowledge else { panic!("fixture must have a retained profile") };
        serde_json::json!({"node_id":profile.node_id,"observed_profile_digest":observation.profile_digest,"reason":"explicit fixture choice, not an execution grant"})
    }

    fn change_profile(node: &mut Node) {
        node.assign_roles(RoleAssignmentRequest {
            operation_id: EntityId::new(), expected_revision: "0".into(),
            roles: vec![NodeRole::Replica], reason: "fixture changes retained capability content".into(),
        }).unwrap();
        node.capability_profile().unwrap();
    }

    fn arm(
        handler: &NodeSelectionHandler, store: &Store, subject: &Identity, value: serde_json::Value,
    ) -> (TaskRecord, EntityId, EntityId, FrozenInput) {
        let (task_id, operation_id, invocation_id) = (EntityId::new(), EntityId::new(), EntityId::new());
        let frozen = handler.freeze(value, subject.clone(), operation_id).unwrap();
        let input_ref = handler.seal(&frozen, "node-selection-record-frozen-input").unwrap();
        let epoch = ControllerEpoch(1);
        let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
        let task = TaskRecord {
            record_schema_version: 1, task_id,
            task_revision: TaskRevision(0), record_revision: RecordRevision(0),
            agent_id: subject.agent_id, conversation_id: subject.agent_id,
            owner: TaskOwner::Task { id: EntityId::new() }, background: false,
            kind: "node.selection.record".into(), definition_version: 1,
            implementation_ref: handler.descriptor().implementation_ref,
            input_ref: input_ref.clone(),
            state: TaskState::Running { checkpoint_ref: None, invocation_id, mode: RunMode::Run, epoch },
            abort_request: None, memos: serde_json::Map::new(), residual_case_ids: vec![],
            created_by_step_id: None, dispatch_eligibility: DispatchEligibility::Ready,
            no_progress_crashes: 0, first_run_at: None, terminal_at: None,
            created_commit: zero, updated_commit: zero,
        };
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task.clone());
        store.commit(batch).unwrap();
        let task_invocation = TaskInvocation {
            task_id, task_revision: TaskRevision(0), record_revision: RecordRevision(0),
            invocation_id, epoch, software_generation: RuntimeGeneration::default(),
            worker_id: EntityId::new(), node_id: EntityId::new(),
        };
        let invocation = NativeTaskInvocation {
            context: CallContext::new_system(RuntimeGeneration::default()),
            task_invocation: task_invocation.clone(), task: task.clone(), operation_id,
            authority: AuthorityRef {
                delegation_id: task_id, delegation_revision: RecordRevision(0), decision_ref: EntityId::new(),
            },
            input: serde_json::Value::Null,
            child_tools: None,
        };
        let (mut operation, mut attempt) = handler.operation_parts(&invocation, &frozen, input_ref).unwrap();
        assert!(store.arm_external_operation(&task_invocation, &mut operation, &mut attempt).unwrap().is_none());
        (task, operation_id, attempt.attempt_id, frozen)
    }

    #[test]
    fn explicit_choice_rejects_model_authority_and_never_substitutes_changed_observation() {
        let (dir, store, handler, subject) = fixture();
        let value = choice(&handler);
        let decision = EntityId::new();
        let mut invalid = value.clone();
        invalid["decision_id"] = serde_json::json!(EntityId::new());
        assert!(handler.freeze(invalid, subject.clone(), decision).is_err());
        let mut invalid = value.clone();
        invalid["reason"] = serde_json::json!("x".repeat(4097));
        assert!(handler.freeze(invalid, subject.clone(), decision).is_err());
        let mut invalid = value.clone();
        invalid["observed_profile_digest"] = serde_json::json!("not-a-digest");
        assert!(handler.freeze(invalid, subject.clone(), decision).is_err());
        let frozen = handler.freeze(value, subject, decision).unwrap();
        assert!(handler.receipt(&frozen).unwrap().is_none());
        change_profile(&mut handler.node.lock().unwrap());
        let receipt = handler.record(&frozen).unwrap();
        assert!(matches!(receipt.outcome, NodeSelectionOutcome::Unavailable { reason: SelectionUnavailable::ObservationChanged }));
        assert_eq!(receipt.selection.node_id, frozen.selection.node_id);
        assert_eq!(receipt.selection.decision_id, decision);
        assert_eq!(receipt.selection.reason, frozen.selection.reason);
        assert!(receipt.observation.unwrap().profile_digest != frozen.selection.observed_profile_digest);
        drop(handler); drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn committed_choice_recovers_original_receipt_without_refresh_or_rearm() {
        let (dir, store, handler, subject) = fixture();
        let (task, operation_id, attempt_id, frozen) = arm(&handler, &store, &subject, choice(&handler));
        let original = handler.record(&frozen).unwrap();
        assert!(matches!(original.outcome, NodeSelectionOutcome::Recorded));
        assert!(store.external_operation_evidence(operation_id).unwrap().is_empty());
        drop(handler); drop(store);
        // Crash after the Node journal commit, before Store E1. The current
        // advertisement changes; reconciliation must not reconsider the choice.
        let mut node = Node::open(&dir.join("node")).unwrap();
        change_profile(&mut node);
        let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
        let recovered = NodeSelectionHandler::new(store.clone(), node);
        let resolved = recovered.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(resolved.state, OperationState::Resolved);
        assert_eq!(resolved.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(resolved.effect_status, EffectStatus::Complete);
        let evidence = store.external_operation_evidence(operation_id).unwrap();
        assert_eq!(evidence.len(), 1);
        let result: serde_json::Value = serde_json::from_slice(&store.cas.read_object(
            &evidence[0].normalized_ref.as_ref().unwrap().digest,
        ).unwrap()).unwrap();
        assert_eq!(result["receipt"], serde_json::to_value(original).unwrap());
        assert_eq!(result["effect_scope"], "local_selection_journal_only");
        assert_eq!(result["dispatch"], "not_dispatched");
        recovered.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(store.external_operation_evidence(operation_id).unwrap().len(), 1);
        assert_eq!(store.external_operation_attempts(operation_id).unwrap().len(), 1);
        assert_eq!(store.external_operation_attempt(operation_id).unwrap().attempt_id, attempt_id);
        drop(recovered); drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn absent_choice_stays_unknown_until_original_receipt_arrives_without_replay() {
        let (dir, store, handler, subject) = fixture();
        let (task, operation_id, attempt_id, frozen) = arm(&handler, &store, &subject, choice(&handler));
        drop(handler);
        let recovered = NodeSelectionHandler::new(store.clone(), Node::open(&dir.join("node")).unwrap());
        let unknown = recovered.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(unknown.state, OperationState::Reconciling);
        assert_eq!(unknown.outcome, None);
        assert_eq!(unknown.effect_status, EffectStatus::Unknown);
        recovered.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(store.external_operation_evidence(operation_id).unwrap().len(), 1);
        assert!(recovered.receipt(&frozen).unwrap().is_none());
        // A delayed original writer, NOT the recovering handler, completes its
        // already-armed local write. The original attempt may now reconcile.
        let mut original_writer = Node::open(&dir.join("node")).unwrap();
        original_writer.record_node_selection_for_subject(
            frozen.selection.clone(), frozen.observer_node_id, &frozen.subject,
        ).unwrap();
        let resolved = recovered.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(resolved.state, OperationState::Resolved);
        assert_eq!(resolved.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(store.external_operation_evidence(operation_id).unwrap().len(), 2);
        assert_eq!(store.external_operation_attempts(operation_id).unwrap().len(), 1);
        assert_eq!(store.external_operation_attempt(operation_id).unwrap().attempt_id, attempt_id);
        drop(original_writer); drop(recovered); drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
