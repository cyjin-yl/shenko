//! `awareness.notice:1` — read the bounded notice material of the installed
//! awareness capability.
//!
//! docs/specs/cognition-units.md#spec-51-2, docs/specs/cognition-units.md#spec-51-3: awareness is a capability, and the harness gives the
//! subject a bounded notice instead of an order. This child is the caller of
//! that host: it reads the host's own residency and notice projection and
//! returns them as an observation. It grants nothing, dispatches nothing and
//! stores no attitude — the material it returns is advisory, and the subject
//! decides what, if anything, to write down.
//!
//! An absent capability is a successful read that says so: the result carries
//! `status: absent` and `resident: false`, and never falls back to documents
//! that merely exist on disk. A caller that treats "the read succeeded" as
//! "the capability is present" is reading it wrong, which is why residency is
//! its own field.

use std::sync::Arc;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_awareness_core::{AwarenessHost, MAX_NOTICES};
use shenko_store::Store;
use shenko_kernel::task::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt},
    ids::*,
    object::*,
    operation::*,
    task::{ImplementationRef, TaskRecord},
};

const NOTICE_CONTRACT: &[u8] = b"shenko.awareness.notice/v1: bounded advisory notices of the installed awareness capability; grants nothing, no external effects";
const NOTICE_KIND: &str = "awareness.notice";
const MAX_NORMALIZED_BYTES: usize = 480 * 1024;

/// One request for the notice projection.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NoticeInput {
    /// How many notices to read at most. Absent means the host's own bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limit: Option<usize>,
}

/// Reads the awareness capability's bounded notice material.
pub struct AwarenessNoticeHandler {
    store: Arc<Store>,
    host: Arc<AwarenessHost>,
}

impl AwarenessNoticeHandler {
    pub fn new(store: Arc<Store>, host: Arc<AwarenessHost>) -> Self {
        Self { store, host }
    }

    fn plugin_id() -> EntityId {
        "5e7c1d92-4a3f-4b86-b1d8-2c9f0a6e7b34"
            .parse()
            .expect("fixed awareness-notice plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-awareness", schema, 1)
        {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: Serialize + ?Sized>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn freeze(&self, value: serde_json::Value) -> Result<NoticeInput> {
        let input: NoticeInput = serde_json::from_value(value)?;
        if let Some(limit) = input.limit {
            ensure!(
                (1..=MAX_NOTICES).contains(&limit),
                "notice limit must be 1..={MAX_NOTICES}"
            );
        }
        Ok(input)
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<NoticeInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "awareness_notice_frozen_input_mismatch"
        );
        let input: NoticeInput = serde_json::from_slice(&bytes)?;
        Ok(input)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: format!("{NOTICE_KIND}.v1"),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "read_only_awareness_notices".into(),
            extra: serde_json::json!({}),
        };
        let resource = self.seal(&bindings, "awareness-notice-binding")?;
        let acceptance = self.seal(
            "the bounded advisory notices this installation currently offers, or its honest absence",
            "awareness-notice-acceptance",
        )?;
        let retry = self.seal(
            "re-read only the frozen notice request under the original armed attempt; never arm again",
            "awareness-notice-retry",
        )?;
        let contract_digest = Digest::blake3(NOTICE_CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:awareness-notice-v1".into(),
            },
            symbol: format!("{NOTICE_KIND}.execute"),
            schema: SchemaRef {
                name: "awareness-notice-observation-v1".into(),
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
            operation_name: format!("{NOTICE_KIND}.v1"),
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

    /// The actual read of the capability host.
    fn run_query(&self, input: &NoticeInput) -> Result<serde_json::Value> {
        let state = self.host.state()?;
        let notices = if state.resident {
            let limit = input.limit.unwrap_or(MAX_NOTICES);
            self.host.notices(limit)?
        } else {
            Vec::new()
        };
        Ok(serde_json::json!({
            "schema_version": 1,
            // A read that honestly found no capability is not a failure; the
            // residency flag and the state reason carry that fact.
            "status": if state.resident { "ok" } else { "absent" },
            "op": "notice",
            "resident": state.resident,
            "selection": state.selection,
            "provider": state.provider,
            "declared": state.declared,
            "state_reason": state.reason,
            "notices": notices,
            "authority": shenko_awareness_core::NOTICE_AUTHORITY,
        }))
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        input: &NoticeInput,
    ) -> Result<OperationRecord> {
        let (result, outcome) = match self.run_query(input) {
            Ok(result) => (result, OperationOutcome::Succeeded),
            Err(error) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "op": "notice",
                    "error": {"code": "awareness_notice_failed", "message": format!("{error:#}")},
                }),
                OperationOutcome::Failed,
            ),
        };
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "awareness-notice-observation")?;
        let normalized = if bytes.len() <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            self.seal(
                &serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "error": {
                        "code": "normalized_result_too_large",
                        "message": "the exact observation is sealed in raw evidence; read fewer notices"
                    }
                }),
                "awareness-notice-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: format!("{NOTICE_KIND}.v1"),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("awareness-notice:{}", attempt.attempt_id)),
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
            invocation.task.kind == NOTICE_KIND && invocation.task.definition_version == 1,
            "awareness_notice_task_identity_mismatch"
        );
        let frozen = self.freeze(std::mem::take(&mut invocation.input))?;
        let input_ref = self.seal(&frozen, "awareness-notice-frozen-input")?;
        let (mut operation, mut attempt) = self.operation_parts(&invocation, input_ref)?;
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
impl NativeTaskHandler for AwarenessNoticeHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: NOTICE_KIND.into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(NOTICE_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-awareness-host/1".into(),
            authority_refs: vec!["awareness.notice/v1".into()],
            prompt_section: Some(
                "awareness.notice/v1 读取本机觉察能力当前提供的有界 notice（材料、适用条件、理由停在哪里、视角）：[delegate:awareness.notice:1] {\"limit\":8}。返回的是供你注意的材料，不是命令，也不授予任何权限；能力没装或没被采纳时如实返回 absent，不要当作已经拥有觉察。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id
                && operation.operation_name == format!("{NOTICE_KIND}.v1"),
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        self.observe(&operation, &attempt, &frozen)
    }
}
