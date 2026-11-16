//! `playground.manage:1` — the subject's own hands on its works.
//!
//! docs/specs/configuration-playground.md#spec-57-6: a personal agent that cannot make or change a thing of its
//! own is a conversation, not a being. This child lets the subject create a
//! work, put new content into it, stage it, publish it, roll it back and
//! list what it has — through the SAME durable artifact lifecycle the Web UI
//! uses, so the facts it produces are the facts a human sees.
//!
//! Every effect is a business fact, not a claim: a version is content
//! addressed, a stage is a candidate manifest, a publish is an explicit
//! adoption, and a rollback appends a new version rather than erasing one.
//! The transitions announce themselves in the subject's own inbox, so the
//! subject perceives what it changed (docs/specs/configuration-playground.md#spec-43-2).
//!
//! The handler owns no authority of its own: the artifact store owns the
//! lifecycle, the capability bound to the installed backend owns execution,
//! and the subject's own step owns the decision to call this at all.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_store::store::Store;
use shenko_store::artifacts::ArtifactOperation;
use shenko_types::commit::CommitRef;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::*;
use shenko_types::object::{ArtifactRef, Digest, ObjectRef, SchemaRef, VersionRef};
use shenko_types::operation::{
    AttemptDispatchStage, AttemptRecord, EffectStatus, OperationContractRef, OperationOutcome,
    OperationRecord, OperationState, ResourceBindings,
};
use shenko_kernel::task::NativeTaskInvocation;
use shenko_types::task::TaskRecord;
use shenko_types::task::NativeTaskDefinition;

use crate::playground::{PlaygroundState, PreparedArtifact, stable_id};

/// The kind of this native child.
pub const PLAYGROUND_MANAGE_KIND: &str = "playground.manage";

/// The contract this child states.
const CONTRACT: &[u8] = b"shenko.playground/manage-v1;durable-artifact-lifecycle;subject-is-the-author";

/// The largest normalized observation this child delivers.
const MAX_NORMALIZED_BYTES: usize = 480 * 1024;

/// What the subject wants done with its works.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManageInput {
    schema_version: u32,
    /// create | put | stage | publish | rollback | list
    op: String,
    /// Which work. Absent for `create` and `list`.
    #[serde(default)]
    artifact_id: Option<String>,
    /// The title of a new work.
    #[serde(default)]
    title: Option<String>,
    /// The mime type of a new work or of new content.
    #[serde(default)]
    mime: Option<String>,
    /// The new content.
    #[serde(default)]
    content: Option<String>,
    /// The revision a rollback returns to.
    #[serde(default)]
    source_revision: Option<u64>,
}

/// The exact observation committed with the effect, before E1 admission.
/// Replaying it preserves the original attempt, receipt identity and time.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManageCheckpoint {
    schema_version: u32,
    operation_id: EntityId,
    attempt_id: EntityId,
    input_digest: Digest,
    /// Historical effect closure, independent of a peer's current head.
    artifact_effect_ref: Option<ObjectRef>,
    evidence: EvidenceReceipt,
    outcome: OperationOutcome,
    effect: EffectStatus,
}

/// The subject's hands on its works.
pub struct PlaygroundManageHandler {
    store: Arc<Store>,
    playground: Arc<PlaygroundState>,
}

impl PlaygroundManageHandler {
    pub fn new(store: Arc<Store>, playground: Arc<PlaygroundState>) -> Self {
        Self { store, playground }
    }

    fn plugin_id() -> EntityId {
        "b6c1d0f4-2a7e-4c19-9d53-6b0f1c8a4e77"
            .parse()
            .expect("fixed playground-manage plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-playground-manage", schema, 1)
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

    /// Freeze the request. What the subject asked for is recorded exactly;
    /// an operation it did not ask for is refused here, not half-performed.
    fn freeze(&self, value: Value) -> Result<ManageInput> {
        let input: ManageInput = serde_json::from_value(value)?;
        ensure!(
            input.schema_version == 1,
            "playground manage input schema must be 1"
        );
        match input.op.as_str() {
            "create" => {
                ensure!(
                    input.artifact_id.is_none(),
                    "create names its own work; do not pass an artifact id"
                );
                ensure!(
                    input.title.as_deref().map(|t| !t.trim().is_empty()).unwrap_or(false),
                    "a new work needs a title"
                );
                ensure!(
                    input.mime.as_deref().map(|m| !m.trim().is_empty()).unwrap_or(false),
                    "a new work needs a mime type"
                );
                ensure!(
                    input.content.is_some(),
                    "a new work needs its content"
                );
            }
            "put" | "stage" | "publish" | "rollback" => {
                ensure!(
                    input.artifact_id.is_some(),
                    "this operation names the work it applies to"
                );
                input.artifact_id.as_deref().context("artifact_id is required")?.parse::<EntityId>()
                    .context("artifact_id must be a UUID")?;
                if input.op == "put" {
                    ensure!(input.content.is_some(), "put needs the new content");
                }
                if input.op == "rollback" {
                    ensure!(
                        input.source_revision.is_some_and(|revision| revision > 0),
                        "rollback names the revision it returns to"
                    );
                }
            }
            "list" => {}
            other => {
                ensure!(
                    false,
                    "unknown playground operation '{other}'; use create, put, stage, publish, rollback or list"
                );
            }
        }
        Ok(input)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "store.artifacts.write/delegated_subject/v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "delegated_subject_artifacts".into(),
            extra: json!({"agent_id": invocation.task.agent_id}),
        };
        let resource = self.seal(&bindings, "playground-binding")?;
        let acceptance = self.seal(
            &"the artifact lifecycle's own durable facts ran: a version is content addressed, a stage is a candidate, a publish is an adoption, a rollback appends",
            "playground-acceptance",
        )?;
        let retry = self.seal(
            &"resolve the original operation by its own identity; never write a second version for the same request and never adopt twice",
            "playground-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:playground-manage-v1".into(),
            },
            symbol: "playground.manage.execute".into(),
            schema: SchemaRef {
                name: "playground-observation-v1".into(),
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
            operation_name: "playground.manage.v1".into(),
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

    /// Seal a proposal and its observation. No artifact row, version file or
    /// inbox fact changes until `commit_prepared` commits the operation binding.
    fn prepare(
        &self,
        input: &ManageInput,
        operation_id: EntityId,
    ) -> Result<(Value, EffectStatus, Option<PreparedArtifact>)> {
        let id = || {
            input
                .artifact_id
                .as_deref()
                .context("operation needs an artifact id")?
                .parse::<EntityId>()
                .context("artifact id is not usable")
        };
        let prepared = match input.op.as_str() {
            "create" => Some(
                self.playground.prepare_put(
                    stable_id(&format!("playground-create/v1:{operation_id}")),
                    input.title.clone().context("create needs a title")?,
                    input.mime.clone().context("create needs a mime type")?,
                    input
                        .content
                        .as_deref()
                        .context("create needs content")?
                        .as_bytes(),
                    true,
                )?,
            ),
            "put" => {
                let id = id()?;
                Some(
                    self.playground.prepare_edit(
                        id,
                        input.title.clone(),
                        input.mime.clone(),
                        input
                            .content
                            .as_deref()
                            .context("put needs content")?
                            .as_bytes(),
                    )?,
                )
            }
            "stage" => Some(self.playground.prepare_stage(
                id()?,
                None,
                Some(&operation_id.to_string()),
            )?),
            "publish" => Some(self.playground.prepare_publish(
                id()?,
                None,
                None,
                Some(&operation_id.to_string()),
            )?),
            "rollback" => Some(
                self.playground.prepare_rollback(
                    id()?,
                    input
                        .source_revision
                        .context("rollback needs source_revision")?,
                )?,
            ),
            "list" => None,
            _ => anyhow::bail!("unknown playground operation"),
        };
        let result = match prepared.as_ref() {
            Some(prepared) => {
                let mut result = json!({"schema_version": 1, "status": "ok", "op": input.op,
                    "artifact_id": prepared.record.artifact_id});
                if let Some(version) = &prepared.version {
                    result["revision"] = json!(version.revision);
                }
                if input.op == "stage" {
                    result["candidate_manifests"] =
                        json!(prepared.record.candidate_manifest_refs.len());
                }
                if input.op == "publish" {
                    result["active_manifest"] = json!(
                        prepared
                            .record
                            .active_manifest_ref
                            .as_ref()
                            .map(|r| r.digest.to_string())
                    );
                }
                result
            }
            None => {
                let works: Vec<Value> = self
                    .playground
                    .list_current()?
                    .iter()
                    .map(|version| {
                        json!({
                            "artifact_id": version.artifact_id, "title": version.title,
                            "mime": version.mime, "revision": version.revision,
                        })
                    })
                    .collect();
                json!({"schema_version": 1, "status": "ok", "op": "list", "works": works})
            }
        };
        let effect = if prepared.is_some() {
            EffectStatus::Complete
        } else {
            EffectStatus::None
        };
        Ok((result, effect, prepared))
    }

    fn prepare_observation(
        &self,
        input: &ManageInput,
        operation_id: EntityId,
    ) -> (
        Value,
        OperationOutcome,
        EffectStatus,
        Option<PreparedArtifact>,
    ) {
        match self.prepare(input, operation_id) {
            Ok((result, effect, prepared)) => {
                (result, OperationOutcome::Succeeded, effect, prepared)
            }
            Err(error) => (
                json!({"schema_version": 1, "status": "failed", "op": input.op,
                "error": {"code": format!("playground_{}_failed", input.op), "message": error.to_string()}}),
                OperationOutcome::Failed,
                EffectStatus::None,
                None,
            ),
        }
    }

    fn checkpoint(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        result: Value,
        outcome: OperationOutcome,
        effect: EffectStatus,
        artifact: Option<&PreparedArtifact>,
    ) -> Result<ObjectRef> {
        let artifact_effect_ref = artifact
            .map(|prepared| {
                self.seal(
                    &json!({
                        "schema_version": 1,
                        "artifact_id": prepared.record.artifact_id,
                        "subject_id": prepared.record.subject_id,
                        "state_revision": prepared.record.state_revision,
                        "state_ref": prepared.record.state_ref,
                        "desired_lifecycle_ref": prepared.record.desired_lifecycle_ref,
                        "active_manifest_ref": prepared.record.active_manifest_ref,
                        "candidate_manifest_refs": prepared.record.candidate_manifest_refs,
                    }),
                    "playground-artifact-effect",
                )
            })
            .transpose()?;
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "playground-observation")?;
        let (normalized, outcome) = if bytes.len() <= MAX_NORMALIZED_BYTES {
            (raw.clone(), outcome)
        } else {
            let oversized = json!({"schema_version": 1, "status": "failed", "error": {
                "code": "normalized_result_too_large",
                "message": "the exact observation is sealed in raw evidence; list fewer works"
            }});
            (
                self.seal(&oversized, "playground-result")?,
                OperationOutcome::Failed,
            )
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: stable_id(&format!("playground-receipt/v1:{}", attempt.attempt_id)),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "playground.manage.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("playground-manage:{}", attempt.attempt_id)),
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
        self.seal(
            &ManageCheckpoint {
                schema_version: 1,
                operation_id: operation.operation_id,
                attempt_id: attempt.attempt_id,
                input_digest: operation.input_digest.clone(),
                artifact_effect_ref,
                evidence,
                outcome,
                effect,
            },
            "playground-operation-checkpoint",
        )
    }

    fn settle_checkpoint(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        reference: &ObjectRef,
    ) -> Result<OperationRecord> {
        let bytes = self.store.cas.read_object(&reference.digest)?;
        ensure!(
            bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
            "playground checkpoint object mismatch"
        );
        let checkpoint: ManageCheckpoint = serde_json::from_slice(&bytes)?;
        ensure!(
            checkpoint.schema_version == 1
                && checkpoint.operation_id == operation.operation_id
                && checkpoint.attempt_id == attempt.attempt_id
                && checkpoint.input_digest == operation.input_digest
                && checkpoint.evidence.operation_id == Some(operation.operation_id)
                && checkpoint.evidence.attempt_id == Some(attempt.attempt_id),
            "playground checkpoint identity mismatch"
        );
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            checkpoint.evidence,
            Some(checkpoint.outcome),
            checkpoint.effect,
        )
    }

    /// No checkpoint is proof of neither success nor failure for operations
    /// armed by older binaries. Preserve Unknown and fence any still-running
    /// pre-commit worker instead of guessing or repeating an effect.
    fn record_unresolved(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
    ) -> Result<OperationRecord> {
        let result = json!({"schema_version": 1, "status": "unknown", "error": {
            "code": "playground_operation_checkpoint_missing",
            "message": "the original operation has no committed outcome checkpoint; its effect is not repeated"
        }});
        let raw = self.seal(&result, "playground-unresolved")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: stable_id(&format!("playground-unresolved/v1:{}", attempt.attempt_id)),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "playground.manage.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("playground-unresolved:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw.clone(),
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(raw),
            observed_at: None,
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            None,
            EffectStatus::Unknown,
        )
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == PLAYGROUND_MANAGE_KIND
                && invocation.task.definition_version == 1,
            "playground_manage_task_identity_mismatch"
        );
        let agent_id = invocation.task.agent_id;
        let (head, _) = self.store.load_subject(agent_id)?;
        let step_id = invocation
            .task
            .created_by_step_id
            .context("playground_manage_has_no_subject_step")?;
        self.store
            .get_step(&head.branch_id, step_id)?
            .context("playground_manage_origin_is_outside_current_subject_branch")?;
        let frozen = self.freeze(std::mem::take(&mut invocation.input))?;
        let input_ref = self.seal(&frozen, "playground-frozen-input")?;
        let (mut operation, mut attempt) = self.operation_parts(&invocation, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            if let Some(reference) = &existing.checkpoint_scope_ref {
                let original = self
                    .store
                    .external_operation_attempt(existing.operation_id)?;
                return self.settle_checkpoint(&existing, &original, reference);
            }
            return Ok(existing);
        }
        let _mutation = self.playground.mutation();
        let (result, outcome, effect, mut prepared) =
            self.prepare_observation(&frozen, operation.operation_id);
        if frozen.op == "create" && let Some(prepared) = &mut prepared {
            prepared.record.creator_task_id = Some(operation.task_id);
        }
        let checkpoint = self.checkpoint(
            &operation,
            &attempt,
            result,
            outcome,
            effect,
            prepared.as_ref(),
        )?;
        let committed = self.playground.commit_prepared(
            prepared.as_ref(),
            Some(ArtifactOperation {
                invocation: &invocation.task_invocation,
                operation_id: operation.operation_id,
                operation_name: &operation.operation_name,
                contract_digest: &operation.contract_ref.contract_digest,
                attempt_id: attempt.attempt_id,
                input_digest: &operation.input_digest,
                checkpoint_ref: &checkpoint,
            }),
        )?;
        self.settle_checkpoint(
            &operation,
            &attempt,
            committed
                .checkpoint_ref
                .as_ref()
                .context("artifact commit has no operation checkpoint")?,
        )
    }


}

#[async_trait]
impl shenko_kernel::task::NativeTaskHandler for PlaygroundManageHandler {
    fn descriptor(&self) -> shenko_kernel::task::NativeTaskDescriptor {
        shenko_kernel::task::NativeTaskDescriptor {
            kind: PLAYGROUND_MANAGE_KIND.into(),
            definition_version: 1,
            implementation_ref: shenko_types::task::ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-child/1".into(),
            authority_refs: vec!["artifact.subject-owned/1".into()],
            prompt_section: Some(
                r#"playground.manage/v1 操作你自己的作品。definition 的 kind 是 "playground.manage"、definition_version 是 1；input 是 JSON 对象，每次必须包含 "schema_version":1 和 "op"。
create：必填 title、mime、content，不传 artifact_id。例如 input={"schema_version":1,"op":"create","title":"笔记","mime":"text/plain","content":"第一版"}。
put：必填 artifact_id、content；可选 title、mime，省略则保留原值。stage：必填 artifact_id，把当前版本暂存为候选。publish：必填 artifact_id，采纳最后一个已暂存候选。
rollback：必填 artifact_id、source_revision（整数），把该旧修订追加为一个新版本，保留其后的历史。例如 input={"schema_version":1,"op":"rollback","artifact_id":"<作品 UUID>","source_revision":1}。
list：input={"schema_version":1,"op":"list"}。不要传未知字段。每次改动都是耐久事实，并回到你自己的收件箱；unknown 表示原操作尚未判明，不应通过新请求盲目重做。"#
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
            operation.task_id == task.task_id && operation.operation_name == "playground.manage.v1",
            "original playground operation mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if let Some(reference) = &operation.checkpoint_scope_ref {
            return self.settle_checkpoint(&operation, &attempt, reference);
        }
        let unresolved = self.record_unresolved(&operation, &attempt)?;
        // An original worker may have committed between our read and E1.
        // Its checkpoint wins; the alternative ordering fences that worker
        // before it can commit any artifact effect.
        if let Some(reference) = &unresolved.checkpoint_scope_ref {
            return self.settle_checkpoint(&unresolved, &attempt, reference);
        }
        Ok(unresolved)
    }
}

/// The definition the subject's tool catalog shows for this child.
pub fn definition() -> NativeTaskDefinition {
    NativeTaskDefinition {
        kind: PLAYGROUND_MANAGE_KIND.into(),
        definition_version: 1,
        input: serde_json::json!({
            "type": "object",
            "required": ["schema_version", "op"],
            "properties": {
                "schema_version": {"const": 1},
                "op": {"enum": ["create", "put", "stage", "publish", "rollback", "list"]},
                "artifact_id": {"type": "string"},
                "title": {"type": "string"},
                "mime": {"type": "string"},
                "content": {"type": "string"},
                "source_revision": {"type": "integer", "minimum": 1}
            },
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
impl PlaygroundManageHandler {
    /// Freeze a request (the test drives the same path the subject does).
    pub(crate) fn freeze_for_test(&self, value: Value) -> Result<ManageInput> {
        self.freeze(value)
    }

    /// Perform the operation and report what it observed.
    pub(crate) fn observe_for_test(
        &self,
        input: &ManageInput,
    ) -> Result<(Value, OperationOutcome, EffectStatus)> {
        let (result, outcome, effect, prepared) = self.prepare_observation(input, EntityId::new());
        if let Some(prepared) = &prepared { self.playground.commit_prepared(Some(prepared), None)?; }
        Ok((result, outcome, effect))
    }
}

#[cfg(test)]
#[path = "playground_manage_tests.rs"]
mod playground_manage_tests;
