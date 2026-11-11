//! media.read — bounded read-only native view over the subject's own durable
//! media facts (docs/specs/contacts.md#spec-15-2, docs/specs/media.md#spec-20).
//!
//! The handler mirrors `errors.read`: the model input is frozen and CAS-sealed
//! before anything is read, the read scope is fixed at freeze time to the
//! caller's own subject agent, the observation is sealed as raw evidence, and
//! replayed invocations reconcile through the original armed attempt. It
//! never returns audio bytes — recordings and transcripts are metadata and
//! text; original audio stays a CAS object resolved by digest.

use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::commit::CommitRef;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::*;
use shenko_types::media::*;
use shenko_types::object::*;
use shenko_types::operation::*;
use shenko_types::task::{ImplementationRef, TaskRecord};

const MEDIA_READ_CONTRACT: &[u8] = b"shenko.media.read/v1: read-only own-subject media facts (stream transcript, recording state and usage, priority inputs); no external effects; no audio bytes";
const MAX_ID_CHARS: usize = 256;
const MAX_NORMALIZED_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MediaReadOp {
    Transcript,
    Recording,
    Priority,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaReadInput {
    op: MediaReadOp,
    provider: String,
    account: String,
    stream_id: String,
    recording_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FrozenMediaQuery {
    Transcript { stream: MediaStreamDescriptor },
    Recording { stream: MediaStreamDescriptor, recording_id: String },
    Priority { stream: MediaStreamDescriptor },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenMediaReadInput {
    schema_version: u32,
    agent_id: EntityId,
    query: FrozenMediaQuery,
}

/// Read-only media facts view. Published through the native task registry by
/// the binary alongside the other built-in handlers.
pub struct MediaReadHandler {
    store: Arc<Store>,
}

impl MediaReadHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn plugin_id() -> EntityId {
        "8c1e2f4a-9b3d-4c58-a7d6-1f0e5b2c8a39"
            .parse()
            .expect("fixed media.read plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-media-read", schema, 1)
        {
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

    /// Freeze the model input. The agent scope is the caller's own subject;
    /// a caller cannot widen the view to another agent's media facts.
    fn freeze(&self, value: serde_json::Value, agent_id: EntityId) -> Result<FrozenMediaReadInput> {
        let MediaReadInput { op, provider, account, stream_id, recording_id } =
            serde_json::from_value(value)?;
        let stream = MediaStreamDescriptor {
            provider: bounded_id("provider", provider)?,
            account: bounded_id("account", account)?,
            stream_id: bounded_id("stream_id", stream_id)?,
        };
        let query = match op {
            MediaReadOp::Transcript => {
                ensure!(recording_id.is_none(), "transcript does not accept a recording pointer");
                FrozenMediaQuery::Transcript { stream }
            }
            MediaReadOp::Recording => {
                let recording_id = bounded_id("recording_id", recording_id.context("recording requires recording_id")?)?;
                FrozenMediaQuery::Recording { stream, recording_id }
            }
            MediaReadOp::Priority => {
                ensure!(recording_id.is_none(), "priority does not accept a recording pointer");
                FrozenMediaQuery::Priority { stream }
            }
        };
        Ok(FrozenMediaReadInput { schema_version: 1, agent_id, query })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenMediaReadInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "media_read_frozen_input_mismatch"
        );
        let frozen: FrozenMediaReadInput = serde_json::from_slice(&bytes)?;
        ensure!(frozen.schema_version == 1, "unsupported_media_input_schema");
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        agent_id: EntityId,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "store.media.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "read_only_own_subject_media".into(),
            extra: serde_json::json!({ "agent_id": agent_id }),
        };
        let resource = self.seal(&bindings, "media-read-binding")?;
        let acceptance = self.seal(
            &"a bounded read of the caller's own committed media facts, or an explicit read failure",
            "media-read-acceptance",
        )?;
        let retry = self.seal(
            &"re-read only the frozen immutable range under the original armed attempt; never arm again",
            "media-read-retry",
        )?;
        let contract_digest = Digest::blake3(MEDIA_READ_CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:media-read-v1".into(),
            },
            symbol: "media.read.execute".into(),
            schema: SchemaRef {
                name: "media-read-observation-v1".into(),
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
            operation_name: "media.read.v1".into(),
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

    fn query(&self, frozen: &FrozenMediaReadInput) -> Result<(serde_json::Value, OperationOutcome)> {
        let agent_id = frozen.agent_id;
        let result = match &frozen.query {
            FrozenMediaQuery::Transcript { stream } => serde_json::json!({
                "schema_version": 1,
                "op": "transcript",
                "agent_id": agent_id,
                "stream": stream,
                "transcript": self.store.media_transcript(stream)?,
            }),
            FrozenMediaQuery::Recording { stream, recording_id } => serde_json::json!({
                "schema_version": 1,
                "op": "recording",
                "agent_id": agent_id,
                "stream": stream,
                "recording": self.store.media_recording(stream, recording_id)?,
                "usage": self.store.media_recording_usage(stream, recording_id)?,
            }),
            FrozenMediaQuery::Priority { stream } => serde_json::json!({
                "schema_version": 1,
                "op": "priority",
                "agent_id": agent_id,
                "stream": stream,
                "inputs": self.store.media_priority_inputs(stream)?,
            }),
        };
        Ok((result, OperationOutcome::Succeeded))
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenMediaReadInput,
    ) -> Result<OperationRecord> {
        let (result, mut outcome) = self.query(frozen)?;
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "media-read-observation")?;
        let normalized = if bytes.len() <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            outcome = OperationOutcome::Failed;
            self.seal(
                &serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "agent_id": frozen.agent_id,
                    "error": {
                        "code": "normalized_result_too_large",
                        "message": "the exact observation is sealed in raw evidence; narrow the read with a segment or recording pointer"
                    }
                }),
                "media-read-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "media.read.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("raw-media:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(outcome),
            EffectStatus::None,
        )
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "media.read" && invocation.task.definition_version == 1,
            "media_read_task_identity_mismatch"
        );
        let (head, _) = self.store.load_subject(invocation.task.agent_id)?;
        let step_id = invocation
            .task
            .created_by_step_id
            .context("media_read_task_has_no_subject_step")?;
        self.store
            .get_step(&head.branch_id, step_id)?
            .context("media_read_task_origin_is_outside_current_subject_branch")?;
        let frozen = self.freeze(
            std::mem::take(&mut invocation.input),
            invocation.task.agent_id,
        )?;
        let input_ref = self.seal(&frozen, "media-read-frozen-input")?;
        let (mut operation, mut attempt) =
            self.operation_parts(&invocation, frozen.agent_id, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }
        self.observe(&operation, &attempt, &frozen)
    }
}

#[async_trait]
impl NativeTaskHandler for MediaReadHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "media.read".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(MEDIA_READ_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-media-reader/1".into(),
            authority_refs: vec!["store.media.read.own_subject/v1".into()],
            prompt_section: Some(
                "media.read/v1 是已安装的只读媒体事实 child。读取你自己主体已提交的媒体事实：\
                 [delegate:media.read:1] {\"op\":\"transcript\",\"provider\":\"…\",\"account\":\"…\",\"stream_id\":\"…\"} 给出流的有效转写与缺口；\
                 op=recording 需 recording_id，给出录音状态与已封存音频用量；op=priority 给出该流的优先输入（打断/停止播放/插话）。\
                 结果只含元数据与文本，不含音频字节；原始音频是 CAS 对象，按摘要取用。说话人是提供方声纹 claim（未知保持未知，confidence 是模型信号）。"
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
            operation.task_id == task.task_id && operation.operation_name == "media.read.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            // Existing unknown/conflict evidence is authoritative. A later
            // successful read must not erase it or create another attempt.
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        self.observe(&operation, &attempt, &frozen)
    }
}

fn bounded_id(field: &str, value: String) -> Result<String> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_ID_CHARS,
        "{field} must be 1..={MAX_ID_CHARS} chars"
    );
    Ok(value)
}
