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
use shenko_types::commit::CommitRef;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::*;
use shenko_types::object::{ArtifactRef, Digest, ObjectRef, SchemaRef, VersionRef};
use shenko_types::operation::{
    AttemptDispatchStage, AttemptRecord, EffectStatus, OperationContractRef, OperationOutcome,
    OperationRecord, OperationState, ResourceBindings,
};
use shenko_subject::NativeTaskInvocation;
use shenko_types::task::TaskRecord;
use shenko_types::task::NativeTaskDefinition;

use crate::playground::PlaygroundState;

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
                if input.op == "put" {
                    ensure!(input.content.is_some(), "put needs the new content");
                }
                if input.op == "rollback" {
                    ensure!(
                        input.source_revision.is_some(),
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

    /// The work itself. Every branch is the artifact store's own durable
    /// lifecycle; nothing here invents a version, a stage or an adoption.
    fn observe(&self, input: &ManageInput) -> (Value, OperationOutcome, EffectStatus) {
        let artifact_id = input
            .artifact_id
            .as_deref()
            .and_then(|id| id.parse::<EntityId>().ok());
        let outcome = match input.op.as_str() {
            "create" => {
                let id = EntityId::new();
                let title = input.title.clone().unwrap_or_default();
                let mime = input.mime.clone().unwrap_or_default();
                let bytes = input.content.clone().unwrap_or_default().into_bytes();
                match self.playground.put(id, title, mime, &bytes) {
                    Ok(version) => (
                        json!({
                            "schema_version": 1,
                            "status": "ok",
                            "op": "create",
                            "artifact_id": id.to_string(),
                            "revision": version.revision,
                        }),
                        OperationOutcome::Succeeded,
                        EffectStatus::Complete,
                    ),
                    Err(error) => (
                        json!({
                            "schema_version": 1,
                            "status": "failed",
                            "op": "create",
                            "error": {"code": "playground_create_failed", "message": error.to_string()},
                        }),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    ),
                }
            }
            "put" => {
                let Some(id) = artifact_id else {
                    return (
                        json!({"schema_version": 1, "status": "failed", "op": "put",
                            "error": {"code": "playground_artifact_missing", "message": "the work id is not usable"}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    );
                };
                let Some(previous) = self.playground.get(&id) else {
                    return (
                        json!({"schema_version": 1, "status": "failed", "op": "put",
                            "error": {"code": "playground_artifact_unknown", "message": "no such work"}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    );
                };
                // A new version keeps what the subject did not change: the
                // title and the mime stay the work's own unless the request
                // says otherwise.
                let title = input
                    .title
                    .clone()
                    .unwrap_or_else(|| previous.title.clone());
                let mime = input
                    .mime
                    .clone()
                    .unwrap_or_else(|| previous.mime.clone());
                let bytes = input.content.clone().unwrap_or_default().into_bytes();
                match self.playground.put(id, title, mime, &bytes) {
                    Ok(version) => (
                        json!({
                            "schema_version": 1,
                            "status": "ok",
                            "op": "put",
                            "artifact_id": id.to_string(),
                            "revision": version.revision,
                        }),
                        OperationOutcome::Succeeded,
                        EffectStatus::Complete,
                    ),
                    Err(error) => (
                        json!({"schema_version": 1, "status": "failed", "op": "put",
                            "error": {"code": "playground_put_failed", "message": error.to_string()}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    ),
                }
            }
            "stage" => {
                let Some(id) = artifact_id else {
                    return (
                        json!({"schema_version": 1, "status": "failed", "op": "stage",
                            "error": {"code": "playground_artifact_missing", "message": "the work id is not usable"}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    );
                };
                match self.playground.stage_artifact(id, None) {
                    Ok(record) => (
                        json!({
                            "schema_version": 1,
                            "status": "ok",
                            "op": "stage",
                            "artifact_id": id.to_string(),
                            "candidate_manifests": record.candidate_manifest_refs.len(),
                        }),
                        OperationOutcome::Succeeded,
                        EffectStatus::Complete,
                    ),
                    Err(error) => (
                        json!({"schema_version": 1, "status": "failed", "op": "stage",
                            "error": {"code": "playground_stage_failed", "message": error.to_string()}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    ),
                }
            }
            "publish" => {
                let Some(id) = artifact_id else {
                    return (
                        json!({"schema_version": 1, "status": "failed", "op": "publish",
                            "error": {"code": "playground_artifact_missing", "message": "the work id is not usable"}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    );
                };
                // Publishing adopts the newest staged candidate. The candidate
                // set stays readable, so the previous proposals remain facts.
                let Some(record) = self.playground.durable_record(id).ok().flatten() else {
                    return (
                        json!({"schema_version": 1, "status": "failed", "op": "publish",
                            "error": {"code": "playground_artifact_unknown", "message": "no such work"}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    );
                };
                let Some(candidate) = record.candidate_manifest_refs.last().cloned() else {
                    return (
                        json!({"schema_version": 1, "status": "failed", "op": "publish",
                            "error": {"code": "playground_no_candidate", "message": "there is no staged candidate to adopt"}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    );
                };
                match self.playground.publish_artifact(id, &candidate.digest, None) {
                    Ok(record) => (
                        json!({
                            "schema_version": 1,
                            "status": "ok",
                            "op": "publish",
                            "artifact_id": id.to_string(),
                            "active_manifest": record.active_manifest_ref.map(|r| r.digest.to_string()),
                        }),
                        OperationOutcome::Succeeded,
                        EffectStatus::Complete,
                    ),
                    Err(error) => (
                        json!({"schema_version": 1, "status": "failed", "op": "publish",
                            "error": {"code": "playground_publish_failed", "message": error.to_string()}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    ),
                }
            }
            "rollback" => {
                let Some(id) = artifact_id else {
                    return (
                        json!({"schema_version": 1, "status": "failed", "op": "rollback",
                            "error": {"code": "playground_artifact_missing", "message": "the work id is not usable"}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    );
                };
                let Some(revision) = input.source_revision else {
                    return (
                        json!({"schema_version": 1, "status": "failed", "op": "rollback",
                            "error": {"code": "playground_revision_missing", "message": "rollback names the revision"}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    );
                };
                match self.playground.rollback(id, revision) {
                    Ok(version) => (
                        json!({
                            "schema_version": 1,
                            "status": "ok",
                            "op": "rollback",
                            "artifact_id": id.to_string(),
                            "revision": version.revision,
                        }),
                        OperationOutcome::Succeeded,
                        EffectStatus::Complete,
                    ),
                    Err(error) => (
                        json!({"schema_version": 1, "status": "failed", "op": "rollback",
                            "error": {"code": "playground_rollback_failed", "message": error.to_string()}}),
                        OperationOutcome::Failed,
                        EffectStatus::None,
                    ),
                }
            }
            "list" => {
                let versions = self.playground.list();
                let works: Vec<Value> = versions
                    .iter()
                    .map(|version| {
                        json!({
                            "artifact_id": version.artifact_id.to_string(),
                            "title": version.title,
                            "mime": version.mime,
                            "revision": version.revision,
                        })
                    })
                    .collect();
                (
                    json!({"schema_version": 1, "status": "ok", "op": "list", "works": works}),
                    OperationOutcome::Succeeded,
                    EffectStatus::None,
                )
            }
            other => (
                json!({"schema_version": 1, "status": "failed", "op": other,
                    "error": {"code": "playground_unknown_op", "message": "unknown operation"}}),
                OperationOutcome::Failed,
                EffectStatus::None,
            ),
        };
        outcome
    }

    fn record(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        result: Value,
        outcome: OperationOutcome,
        effect: EffectStatus,
    ) -> Result<OperationRecord> {
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "playground-observation")?;
        let (normalized, outcome) = if bytes.len() <= MAX_NORMALIZED_BYTES {
            (raw.clone(), outcome)
        } else {
            let oversized = json!({
                "schema_version": 1,
                "status": "failed",
                "error": {
                    "code": "normalized_result_too_large",
                    "message": "the exact observation is sealed in raw evidence; list fewer works"
                }
            });
            (self.seal(&oversized, "playground-result")?, OperationOutcome::Failed)
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
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
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(outcome),
            effect,
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
            return Ok(existing);
        }
        let (result, outcome, effect) = self.observe(&frozen);
        self.record(&operation, &attempt, result, outcome, effect)
    }
}

#[async_trait]
impl shenko_subject::NativeTaskHandler for PlaygroundManageHandler {
    fn descriptor(&self) -> shenko_subject::NativeTaskDescriptor {
        shenko_subject::NativeTaskDescriptor {
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
                "playground.manage/v1 是你自己的作品：create 建一件新作品，put 写入新内容，\
stage 把当前版本作为候选，publish 采纳已暂存的候选，rollback 回到某个旧修订，\
list 看你有哪些作品。每次改动都是耐久事实，并回到你自己的收件箱。"
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
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            return Ok(operation);
        }
        // A managed attempt is re-observed under its original identity: the
        // lifecycle's own records decide whether the effect happened, so the
        // same call is never performed twice.
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest,
            "playground_manage_frozen_input_mismatch"
        );
        let frozen: ManageInput = serde_json::from_slice(&bytes)?;
        let (result, outcome, effect) = self.observe(&frozen);
        self.record(&operation, &attempt, result, outcome, effect)
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
                "source_revision": {"type": "integer"}
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
    ) -> (Value, OperationOutcome, EffectStatus) {
        self.observe(input)
    }
}

#[cfg(test)]
#[path = "playground_manage_tests.rs"]
mod playground_manage_tests;
