//! The `update.control` native handler — the subject/child route into the
//! activation machinery.
//!
//! The model never touches files or the selector directly: it requests one
//! of the four operations, each of which runs through the frozen-input /
//! operation / attempt / evidence contract like every other native handler.
//! The prompt section states what exists; it does not prescribe a scenario.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;

use shenko_store::Store;
use shenko_kernel::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::commit::CommitRef;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::{ArtifactRef, Digest, ObjectRef, SchemaRef, VersionRef};
use shenko_types::operation::{
    AttemptDispatchStage, AttemptRecord, EffectStatus, OperationContractRef,
    OperationOutcome, OperationRecord, OperationState, ResourceBindings,
};
use shenko_types::task::{ImplementationRef, TaskRecord};

use crate::build::{BuildProfile, CargoBuildProvider};
use crate::handoff::{
    CandidateSource, HandoffParticipantRegistry, PrepareRequest, PrimaryClaimSource, UpdateService,
};
use crate::slots;

const UPDATE_CONTRACT: &[u8] = b"shenko.update.control/v1: prepare|activate|rollback|status of a software-generation hand-over; real files, real selector CAS, real evidence; no network, no real outbox replay, no second subject loop";

#[async_trait]
pub trait ProcessHandoffControl: Send + Sync {
    async fn request(&self, command: crate::cli::UpdateCommand) -> Result<serde_json::Value>;
}

/// One update operation requested by the model.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateInput {
    op: UpdateOp,
    generation: Option<String>,
    source_dir: Option<String>,
    artifact: Option<String>,
    profile: Option<String>,
    target: Option<String>,
    extra_args: Option<Vec<String>>,
    activation_id: Option<EntityId>,
    expected_selector_revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum UpdateOp {
    Status,
    Prepare,
    Activate,
    Rollback,
}

/// The frozen, schema-versioned form of the request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FrozenUpdateInput {
    schema_version: u32,
    agent_id: EntityId,
    op: UpdateOp,
    generation: Option<String>,
    source_dir: Option<String>,
    artifact: Option<String>,
    profile: Option<String>,
    target: Option<String>,
    extra_args: Option<Vec<String>>,
    activation_id: Option<EntityId>,
    expected_selector_revision: Option<u64>,
    digest: String,
}

/// The native `update.control` handler.
#[derive(Clone)]
pub struct UpdateControlHandler {
    store: Arc<Store>,
    data_dir: PathBuf,
    participants: Arc<HandoffParticipantRegistry>,
    claim_source: Option<Arc<dyn PrimaryClaimSource>>,
    process_control: Arc<dyn ProcessHandoffControl>,
}

impl UpdateControlHandler {
    pub fn new(
        store: Arc<Store>,
        data_dir: PathBuf,
        participants: Arc<HandoffParticipantRegistry>,
        claim_source: Option<Arc<dyn PrimaryClaimSource>>,
        process_control: Arc<dyn ProcessHandoffControl>,
    ) -> Self {
        Self {
            store,
            data_dir,
            participants,
            claim_source,
            process_control,
        }
    }

    fn plugin_id() -> EntityId {
        "6c1d4f2e-9a3b-4e7d-8c5f-1a2b3c4d5e6f"
            .parse()
            .expect("fixed update.control plugin UUID")
    }

    fn service(&self, _agent_id: EntityId) -> Result<UpdateService> {
        let live_slot = slots::resolve_live_slot(&self.data_dir)?
            .unwrap_or_else(|| self.data_dir.clone());
        let mut service = UpdateService::new(
            self.data_dir.clone(),
            live_slot,
            Arc::clone(&self.store),
            Arc::clone(&self.participants),
            Arc::new(CargoBuildProvider::default()),
        );
        if let Some(source) = &self.claim_source {
            service = service.with_claim_source(Arc::clone(source));
        }
        Ok(service)
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-update-control", schema, 1)
        {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: serde::Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn freeze(&self, value: serde_json::Value, agent_id: EntityId) -> Result<FrozenUpdateInput> {
        let input: UpdateInput = serde_json::from_value(value)?;
        let digest = Digest::blake3(
            serde_json::to_vec(&serde_json::json!({
                "v": 1,
                "agent_id": agent_id,
                "op": input.op,
                "generation": input.generation,
                "source_dir": input.source_dir,
                "artifact": input.artifact,
                "profile": input.profile,
                "target": input.target,
                "extra_args": input.extra_args,
                "activation_id": input.activation_id,
                "expected_selector_revision": input.expected_selector_revision,
            }))?
            .as_slice(),
        );
        Ok(FrozenUpdateInput {
            schema_version: 1,
            agent_id,
            op: input.op,
            generation: input.generation,
            source_dir: input.source_dir,
            artifact: input.artifact,
            profile: input.profile,
            target: input.target,
            extra_args: input.extra_args,
            activation_id: input.activation_id,
            expected_selector_revision: input.expected_selector_revision,
            digest: digest.to_string(),
        })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenUpdateInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "update_control_frozen_input_mismatch"
        );
        let frozen: FrozenUpdateInput = serde_json::from_slice(&bytes)?;
        ensure!(frozen.schema_version == 1, "unsupported_update_input_schema");
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        agent_id: EntityId,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "shenko.activation.store.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "write_activation_records_and_candidate_slots".into(),
            extra: serde_json::json!({
                "agent_id": agent_id,
                "data_dir": self.data_dir.to_string_lossy(),
            }),
        };
        let resource = self.seal(&bindings, "update-control-binding")?;
        let acceptance = self.seal(
            &"exactly one activation progresses per accepted operation; the selector revision is the CAS token; a failure or unknown outcome is recorded, never retried blindly",
            "update-control-acceptance",
        )?;
        let retry = self.seal(
            &"re-read the committed state; a selector revision that already moved refuses the retry; a crashed attempt is reconciled from its own receipt",
            "update-control-retry",
        )?;
        let contract_digest = Digest::blake3(UPDATE_CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:update-control-v1".into(),
            },
            symbol: "update.control.execute".into(),
            schema: SchemaRef {
                name: "update-control-operation-v1".into(),
                major: 1,
                digest: contract_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: shenko_types::ids::RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: "update.control.v1".into(),
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

    async fn execute_frozen(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenUpdateInput,
    ) -> Result<OperationRecord> {
        let service = self.service(frozen.agent_id)?;
        match frozen.op {
            UpdateOp::Status => {
                let status = service.status(frozen.agent_id)?;
                self.observe(
                    operation,
                    attempt,
                    &serde_json::json!({"status": status}),
                    Some(OperationOutcome::Succeeded),
                    EffectStatus::None,
                )
            }
            UpdateOp::Prepare => {
                let generation = frozen
                    .generation
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("prepare requires `generation`"))?;
                let source = match (frozen.source_dir.clone(), frozen.artifact.clone()) {
                    (Some(source_dir), None) => CandidateSource::Build {
                        source_dir: PathBuf::from(source_dir),
                        profile: match frozen.profile.as_deref() {
                            Some("debug") => BuildProfile::Debug,
                            _ => BuildProfile::Release,
                        },
                        target: frozen.target.clone(),
                        extra_args: frozen.extra_args.clone().unwrap_or_default(),
                    },
                    (None, Some(artifact)) => CandidateSource::Prebuilt {
                        path: PathBuf::from(artifact),
                    },
                    _ => {
                        bail!("prepare needs exactly one of `source_dir` or `artifact`")
                    }
                };
                let claim = match &self.claim_source {
                    Some(source) => source.committed_primary_claim()?,
                    None => None,
                };
                let record = service
                    .prepare(PrepareRequest {
                        agent_id: frozen.agent_id,
                        generation: shenko_types::ids::RuntimeGeneration(generation),
                        source,
                        primary_claim: claim,
                    })
                    .await?;
                self.observe(
                    operation,
                    attempt,
                    &serde_json::json!({
                        "activation_id": record.activation_id,
                        "phase": record.phase.name(),
                        "to_generation": record.to_generation.0,
                        "imported_through": record.imported_through,
                        "smoke_evidence_ref": record.smoke_evidence_ref,
                    }),
                    Some(OperationOutcome::Succeeded),
                    EffectStatus::Complete,
                )
            }
            UpdateOp::Activate => {
                let activation_id = frozen
                    .activation_id
                    .ok_or_else(|| anyhow::anyhow!("activate requires `activation_id`"))?;
                let expected = frozen
                    .expected_selector_revision
                    .ok_or_else(|| anyhow::anyhow!("activate requires `expected_selector_revision`"))?;
                let accepted = self.process_control.request(crate::cli::UpdateCommand::Activate {
                    activation: activation_id, expected_selector_revision: expected,
                }).await;
                return match accepted {
                    Ok(value) => self.observe(operation, attempt, &value, None, EffectStatus::Unknown),
                    Err(error) => self.observe(operation, attempt, &serde_json::json!({"control_request_error": format!("{error:#}")}), None, EffectStatus::Unknown),
                };
            }
            UpdateOp::Rollback => {
                let activation_id = frozen
                    .activation_id
                    .ok_or_else(|| anyhow::anyhow!("rollback requires `activation_id`"))?;
                let expected = frozen
                    .expected_selector_revision
                    .ok_or_else(|| anyhow::anyhow!("rollback requires `expected_selector_revision`"))?;
                let accepted = self.process_control.request(crate::cli::UpdateCommand::Rollback {
                    activation: activation_id, expected_selector_revision: expected,
                }).await;
                return match accepted {
                    Ok(value) => self.observe(operation, attempt, &value, None, EffectStatus::Unknown),
                    Err(error) => self.observe(operation, attempt, &serde_json::json!({"control_request_error": format!("{error:#}")}), None, EffectStatus::Unknown),
                };
            }
        }
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        result: &serde_json::Value,
        outcome: Option<OperationOutcome>,
        effect_status: EffectStatus,
    ) -> Result<OperationRecord> {
        let raw = self.seal(result, "update-control-result")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "update.control.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("update-control:{}:{}", attempt.attempt_id, raw.digest)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: None,
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        // The orchestration receipt is deliberately later than w1. Its
        // original producer/attempt remains unchanged, but E1 belongs in the
        // newly selected store, not stranded in the retired writer's copy.
        let selected_slot = slots::resolve_live_slot(&self.data_dir)?
            .unwrap_or_else(|| self.data_dir.clone());
        let receipt_store = Store::open(selected_slot.join(crate::SLOT_DB_FILENAME), self.data_dir.join("cas"))?;
        receipt_store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            outcome,
            effect_status,
        )
    }
}

#[async_trait]
impl NativeTaskHandler for UpdateControlHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "update.control".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(UPDATE_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-update-control/1".into(),
            authority_refs: vec![
                "activation.selector.write/v1".into(),
                "activation.phase.advance/v1".into(),
                "candidate_slot.build.write/v1".into(),
            ],
            prompt_section: Some(
                "update.control/v1 是已安装的软件代际交接 child。\
                 查询现状：[delegate:update.control:1] {\"op\":\"status\"}。\
                 准备候选：{\"op\":\"prepare\",\"generation\":\"<新代际名>\",\"source_dir\":\"<源码目录>\"} 或 {\"op\":\"prepare\",\"generation\":\"<新代际名>\",\"artifact\":\"<已构建产物路径>\"}；它会在独立副本上构建/复制/迁移/烟测并产生一条 activation 记录，绝不覆盖唯一运行副本。\
                 切换：{\"op\":\"activate\",\"activation_id\":\"<id>\",\"expected_selector_revision\":<当前 revision>}；这是唯一换权点，旧代际的写入者会被 fence，新事实保留。\
                 回滚：{\"op\":\"rollback\",\"activation_id\":\"<id>\",\"expected_selector_revision\":<当前 revision>}；它创建一条新的 activation 并把新代际的新事实导入旧副本。\
                 四个操作都不发真实消息、不驱动真实 seat、不启动第二条主体循环；是否更新由你自主决定。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == "update.control.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if !matches!(operation.state, OperationState::InFlight | OperationState::Reconciling)
            || !matches!(attempt.dispatch_stage, AttemptDispatchStage::Armed | AttemptDispatchStage::Unknown)
        {
            // Existing unknown/conflict evidence is authoritative. A later
            // successful read must not erase it or create another attempt.
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        if matches!(frozen.op, UpdateOp::Activate | UpdateOp::Rollback) {
            let original_id = frozen.activation_id.context("original activation identity missing")?;
            let expected = frozen.expected_selector_revision.context("original selector expectation missing")?;
            let original = self.store.activation_record(original_id)?.context("original activation record missing")?;
            if let Some(selected) = self.store.activation_selector(frozen.agent_id)? {
                let matches = selected.selector_revision == expected + 1 && match frozen.op {
                    UpdateOp::Activate => selected.origin == shenko_types::activation::ActivationOrigin::Activation { activation_id: original_id }
                        && selected.generation == original.to_generation,
                    UpdateOp::Rollback => {
                        if selected.generation != original.from_generation { false } else {
                            match &selected.origin {
                                shenko_types::activation::ActivationOrigin::Activation { activation_id } => {
                                    let rollback = self.store.activation_record(*activation_id)?.context("rollback selection record missing")?;
                                    let closure: serde_json::Value = serde_json::from_slice(&self.store.cas.read_object(&rollback.recovery_closure_ref.digest)?)?;
                                    closure.get("rollback_of").and_then(|value| value.as_str()) == Some(original_id.to_string().as_str())
                                }
                                _ => false,
                            }
                        }
                    }
                    _ => false,
                };
                if matches {
                    if let Some(launch) = self.store.activation_launch_for_selector(frozen.agent_id, selected.selector_revision)? {
                        return self.observe(&operation, &attempt, &serde_json::json!({"selector": selected, "launch_receipt": launch}),
                            Some(OperationOutcome::Succeeded), EffectStatus::Complete);
                    }
                    return Ok(operation);
                }
            }
        }
        // Reconciliation never re-runs the effect: a crashed activation is
        // continued from the recovery read of the committed state.
        match frozen.op {
            UpdateOp::Status => self.execute_frozen(&operation, &attempt, &frozen).await,
            _ => {
                let service = self.service(frozen.agent_id)?;
                let report = service.recover(frozen.agent_id)?;
                // A live control orchestrator may be waiting for the caller
                // to leave its step. Reading status must not settle that
                // original operation while it is still executing.
                if report.open_activation.as_ref().is_some_and(|record| !record.phase.is_terminal()) {
                    return Ok(operation);
                }
                self.observe(
                    &operation,
                    &attempt,
                    &serde_json::json!({"recovery": report}),
                    None,
                    operation.effect_status,
                )
            }
        }
    }
}

impl UpdateControlHandler {
    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "update.control" && invocation.task.definition_version == 1,
            "update_control_task_identity_mismatch"
        );
        let frozen = self.freeze(std::mem::take(&mut invocation.input), invocation.task.agent_id)?;
        let input_ref = self.seal(&frozen, "update-control-frozen-input")?;
        let (mut operation, mut attempt) =
            self.operation_parts(&invocation, frozen.agent_id, input_ref)?;
        if let Some(existing) =
            self.store
                .arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)?
        {
            return Ok(existing);
        }
        if matches!(frozen.op, UpdateOp::Activate | UpdateOp::Rollback) {
            // Return the armed operation before waiting for a subject
            // boundary. The current native worker can now finish/park, so
            // the independent control task cannot deadlock on its caller.
            let controller = self.clone();
            let pending = operation.clone();
            tokio::spawn(async move {
                if let Err(error) = controller.execute_frozen(&operation, &attempt, &frozen).await {
                    if let Err(receipt_error) = controller.observe(
                        &operation, &attempt,
                        &serde_json::json!({"control_error": format!("{error:#}")}),
                        None, EffectStatus::Unknown,
                    ) {
                        tracing::error!(%receipt_error, %error, "update control outcome could not be recorded");
                    }
                }
            });
            return Ok(pending);
        }
        self.execute_frozen(&operation, &attempt, &frozen).await
    }
}
