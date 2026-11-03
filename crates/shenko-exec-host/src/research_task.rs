//! Research lineage handlers — `research.reset`, `research.candidate`,
//! `research.bank` — and the retention handler `retention.apply`.
//!
//! These are real native children: each executes its store write inside the
//! T1-armed attempt boundary, records the sealed result as E1 evidence, and
//! resolves the operation. Every write is idempotent on the identity the
//! caller freezes, so a crash between the durable write and the durable
//! evidence lets `reconcile` re-drive the same fact without creating a second
//! epoch, candidate, bank entry or tombstone.

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_store::research::{
    BankEntryKind, BankSelection, InterventionBankEntry, ResearchBudget, ResearchCandidate,
    ResearchClosure, ResearchConstraint, ResearchEffect, ResearchEnvironment,
    ResearchEpochStatus, ResearchResetTrigger,
};
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::commit::CommitRef;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::*;
use shenko_types::object::*;
use shenko_types::operation::*;
use shenko_types::task::{ImplementationRef, TaskRecord};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Shared operation/evidence boilerplate.
// ---------------------------------------------------------------------------

fn seal(store: &Store, value: &impl Serialize, schema: &str) -> Result<ObjectRef> {
    match store.cas.seal_object(
        &serde_json::to_vec(value)?,
        "application/json",
        "host-native-research",
        schema,
        1,
    ) {
        Ok(sealed) => Ok(sealed.object),
        Err(error) => {
            store.close_external_writer();
            Err(error.into())
        }
    }
}

/// The operation/attempt pair every research handler arms before its write.
#[allow(clippy::too_many_arguments)]
fn operation_parts(
    store: &Store,
    invocation: &NativeTaskInvocation,
    contract: &[u8],
    plugin_id: EntityId,
    operation_name: &str,
    symbol: &str,
    schema_name: &str,
    capability_level: &str,
    input_ref: ObjectRef,
) -> Result<(OperationRecord, AttemptRecord)> {
    let bindings = ResourceBindings {
        provider: "store.research.v1".into(),
        account_identity: String::new(),
        runtime_node: invocation.task_invocation.node_id.to_string(),
        mount_generation: 0,
        capability_level: capability_level.into(),
        extra: serde_json::json!({}),
    };
    let resource = seal(store, &bindings, "research-binding")?;
    let acceptance = seal(
        store,
        &"one idempotent research write recorded in the durable ledger, or an explicit rejection",
        "research-acceptance",
    )?;
    let retry = seal(
        store,
        &"re-drive only the frozen idempotent write under the original armed attempt; never arm again",
        "research-retry",
    )?;
    let contract_digest = Digest::blake3(contract);
    let version = VersionRef {
        artifact: ArtifactRef {
            digest: contract_digest.clone(),
            locator: format!("builtin:{operation_name}"),
        },
        symbol: symbol.into(),
        schema: SchemaRef {
            name: schema_name.into(),
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
        operation_name: operation_name.into(),
        input_schema_version: 1,
        output_schema_version: 1,
        input_digest: input_ref.digest.clone(),
        final_input_ref: input_ref,
        contract_ref: OperationContractRef {
            plugin_id,
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

/// Arm T1, run the idempotent write, and record the sealed result as the E1
/// evidence that settles the operation. `write` receives the frozen input and
/// returns `(result_json, outcome)`.
pub(crate) fn run_recorded<T, F>(
    store: &Store,
    invocation: &NativeTaskInvocation,
    contract: &[u8],
    plugin_id: EntityId,
    operation_name: &str,
    symbol: &str,
    schema_name: &str,
    capability_level: &str,
    frozen: &T,
    write: F,
) -> Result<OperationRecord>
where
    T: Serialize,
    F: FnOnce(&Store, &T) -> Result<(serde_json::Value, OperationOutcome)>,
{
    let input_ref = seal(store, frozen, &format!("{operation_name}-frozen-input"))?;
    let (mut operation, mut attempt) = operation_parts(
        store,
        invocation,
        contract,
        plugin_id,
        operation_name,
        symbol,
        schema_name,
        capability_level,
        input_ref,
    )?;
    if let Some(existing) =
        store.arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)?
    {
        return Ok(existing);
    }
    let (result, outcome) = match write(store, frozen) {
        Ok((result, outcome)) => (result, outcome),
        Err(error) => {
            // The write itself failed: record a failed operation with the
            // error as the normalized result so the subject sees the real
            // rejection instead of a silent park.
            let failure = serde_json::json!({
                "schema_version": 1,
                "status": "failed",
                "error": {"code": "research_write_rejected", "message": format!("{error:#}")},
            });
            (failure, OperationOutcome::Failed)
        }
    };
    record_outcome(
        store,
        &operation,
        &attempt,
        operation_name,
        &result,
        outcome,
        EffectStatus::Complete,
    )
}

/// Seal the result and record it as the final evidence of the armed attempt.
pub(crate) fn record_outcome(
    store: &Store,
    operation: &OperationRecord,
    attempt: &AttemptRecord,
    source_id: &str,
    result: &serde_json::Value,
    outcome: OperationOutcome,
    effect_status: EffectStatus,
) -> Result<OperationRecord> {
    let raw = seal(store, result, &format!("{source_id}-observation"))?;
    let evidence = EvidenceReceipt {
        record_schema_version: 1,
        receipt_id: EntityId::new(),
        operation_id: Some(operation.operation_id),
        attempt_id: Some(attempt.attempt_id),
        source_id: source_id.into(),
        source_instance_id: attempt.node_id.to_string(),
        source_event_key: Some(format!("research:{}:{}", source_id, attempt.attempt_id)),
        producer_epoch: Some(attempt.epoch.0),
        producer_schema_version: Some("1".into()),
        raw_ref: raw.clone(),
        decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
        decode_status: DecodeStatus::Decoded,
        // These are ledger writes, not remote observations.
        observation_level: None,
        normalized_ref: Some(raw),
        observed_at: Some(chrono::Utc::now()),
        received_commit: operation.prepared_commit,
    };
    store.record_external_evidence(
        operation.operation_id,
        attempt.attempt_id,
        evidence,
        Some(outcome),
        effect_status,
    )
}

/// Reconcile: an already-resolved operation is authoritative; an Armed attempt
/// is re-driven through the same idempotent write; unknown/conflict evidence
/// is left alone.
async fn reconcile_write<T, F>(
    store: &Store,
    task: &TaskRecord,
    operation_id: EntityId,
    operation_name: &str,
    frozen: impl Fn(&OperationRecord) -> Result<T>,
    write: F,
) -> Result<OperationRecord>
where
    T: Serialize,
    F: FnOnce(&Store, &T) -> Result<(serde_json::Value, OperationOutcome)>,
{
    let operation = store.external_operation(operation_id)?;
    ensure!(
        operation.task_id == task.task_id,
        "original_operation_contract_mismatch"
    );
    if operation.state == OperationState::Resolved {
        return Ok(operation);
    }
    let attempt = store.external_operation_attempt(operation_id)?;
    if operation.state != OperationState::InFlight
        || attempt.dispatch_stage != AttemptDispatchStage::Armed
    {
        return Ok(operation);
    }
    let frozen = frozen(&operation)?;
    let (result, outcome) = write(store, &frozen)?;
    record_outcome(
        store,
        &operation,
        &attempt,
        operation_name,
        &result,
        outcome,
        EffectStatus::Complete,
    )
}

pub(crate) fn read_frozen<T: for<'de> Deserialize<'de>>(
    store: &Store,
    operation: &OperationRecord,
) -> Result<T> {
    let object = &operation.final_input_ref;
    let bytes = store.cas.read_object(&object.digest)?;
    ensure!(
        bytes.len() as u64 == object.bytes
            && Digest::blake3(&bytes) == object.digest
            && operation.input_digest == object.digest,
        "research_frozen_input_mismatch"
    );
    serde_json::from_slice(&bytes).context("research frozen input decode")
}

// ---------------------------------------------------------------------------
// research.reset
// ---------------------------------------------------------------------------

const RESET_CONTRACT: &[u8] = b"shenko.host.research-reset/v1: one idempotent epoch open/reset recorded in the durable ledger; previous epochs stay readable; no user data is deleted";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetInput {
    request_id: EntityId,
    lineage_id: Option<EntityId>,
    op: ResetOp,
    trigger: Option<ResearchResetTrigger>,
    reason: String,
    constraints: Vec<ResearchConstraint>,
    budget: ResearchBudget,
    effects: Vec<ResearchEffect>,
    environment: ResearchEnvironment,
    best_candidate_id: Option<EntityId>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResetOp {
    Open,
    Reset,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenReset {
    schema_version: u32,
    request_id: EntityId,
    lineage_id: Option<EntityId>,
    op: ResetOp,
    trigger: Option<ResearchResetTrigger>,
    reason: String,
    constraints: Vec<ResearchConstraint>,
    budget: ResearchBudget,
    effects: Vec<ResearchEffect>,
    environment: ResearchEnvironment,
    best_candidate_id: Option<EntityId>,
}

pub struct ResearchResetHandler {
    store: Arc<Store>,
}

impl ResearchResetHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn plugin_id() -> EntityId {
        "7a3b8c40-2d1e-4f5a-9c6b-1e2f3a4b5c6d"
            .parse()
            .expect("fixed built-in research plugin UUID")
    }

    fn freeze(&self, input: &serde_json::Value) -> Result<FrozenReset> {
        let input = ResetInput::deserialize(input)?;
        ensure!(!input.reason.is_empty(), "reset reason must not be empty");
        ensure!(
            input.reason.len() <= 4096,
            "reset reason exceeds 4096 bytes"
        );
        ensure!(
            input.constraints.len() <= 64,
            "reset closure carries more than 64 constraints"
        );
        for constraint in &input.constraints {
            ensure!(
                !constraint.statement.trim().is_empty() && constraint.statement.len() <= 1024,
                "each reset constraint must be non-empty and at most 1024 bytes"
            );
        }
        for effect in &input.effects {
            ensure!(
                !effect.description.trim().is_empty() && effect.description.len() <= 1024,
                "each carried effect must be non-empty and at most 1024 bytes"
            );
        }
        Ok(FrozenReset {
            schema_version: 1,
            request_id: input.request_id,
            lineage_id: input.lineage_id,
            op: input.op,
            trigger: input.trigger,
            reason: input.reason,
            constraints: input.constraints,
            budget: input.budget,
            effects: input.effects,
            environment: input.environment,
            best_candidate_id: input.best_candidate_id,
        })
    }

    fn closure_of(frozen: &FrozenReset) -> ResearchClosure {
        ResearchClosure {
            constraints: frozen.constraints.clone(),
            budget: frozen.budget.clone(),
            effects: frozen.effects.clone(),
            environment: frozen.environment.clone(),
        }
    }

    fn trigger_of(frozen: &FrozenReset) -> ResearchResetTrigger {
        frozen.trigger.unwrap_or(ResearchResetTrigger::SubjectRequest)
    }

    fn write(&self, frozen: &FrozenReset, task_id: EntityId) -> Result<(serde_json::Value, OperationOutcome)> {
        let closure = Self::closure_of(frozen);
        match frozen.op {
            ResetOp::Open => {
                let lineage_id = frozen
                    .lineage_id
                    .context("open requires the lineage_id to open")?;
                let epoch =
                    self.store
                        .open_research_lineage(frozen.request_id, lineage_id, Some(task_id), &closure)?;
                let brief = self.store.research_worker_brief(epoch.epoch_id)?;
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "opened",
                        "epoch": epoch_json(&epoch, &brief),
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
            ResetOp::Reset => {
                let lineage_id = frozen
                    .lineage_id
                    .context("reset requires the lineage_id to reset")?;
                let outcome = self.store.reset_research_epoch(
                    frozen.request_id,
                    lineage_id,
                    Self::trigger_of(frozen),
                    &frozen.reason,
                    &closure,
                    frozen.best_candidate_id,
                )?;
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "reset",
                        "previous_epoch": epoch_json(&outcome.previous_epoch, &None),
                        "epoch": epoch_json(&outcome.epoch, &Some(outcome.brief)),
                        "validation": serde_json::to_value(&outcome.validation)?,
                        "carried_candidate_ids": outcome
                            .carried_candidates
                            .iter()
                            .map(|candidate| candidate.candidate_id.to_string())
                            .collect::<Vec<_>>(),
                        "carried_bank_entry_ids": outcome
                            .carried_bank_entry_ids
                            .iter()
                            .map(|id| id.to_string())
                            .collect::<Vec<_>>(),
                        "excluded_failure_entries": outcome.excluded_failure_entries,
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
        }
    }
}

#[async_trait]
impl NativeTaskHandler for ResearchResetHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "research.reset".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(RESET_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-research-reset/1".into(),
            authority_refs: vec!["store.research.reset.own_subject/v1".into()],
            prompt_section: Some(
                "research.reset/v1 打开或重置研究 lineage。输入 op=open|reset、request_id、lineage_id、reason、constraints:[{id,statement,source?}]、budget:{max_tokens?,max_duration_secs?,max_cost_units?,notes?}、effects:[{effect_id,description,external,recorded_at_ms}]、environment:{environment_id,facts,toolchain,dataset_versions,reproducible}；reset 可指定 trigger 与 best_candidate_id。环境与效果必须来自实际事实，不以版本标签代替。返回新 epoch、密封简报、最佳候选校验和保留的约束/预算/效果。旧 epoch 冻结后保持可读，不删除用户数据。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let frozen = self.freeze(&invocation.input)?;
        let task_id = invocation.task_invocation.task_id;
        run_recorded(
            &self.store,
            &invocation,
            RESET_CONTRACT,
            Self::plugin_id(),
            "research.reset.v1",
            "research.reset.execute",
            "research-reset-observation-v1",
            "research_reset_own_subject_lineage",
            &frozen,
            move |_store, frozen| self.write(frozen, task_id),
        )
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_write(
            &self.store,
            task,
            operation_id,
            "research.reset.v1",
            |operation| read_frozen::<FrozenReset>(&self.store, operation),
            move |_store, frozen| self.write(frozen, task.task_id),
        )
        .await
    }
}

fn epoch_json(
    epoch: &shenko_store::research::ResearchEpoch,
    brief: &Option<shenko_store::research::ResearchWorkerBrief>,
) -> serde_json::Value {
    serde_json::json!({
        "epoch_id": epoch.epoch_id.to_string(),
        "epoch_no": epoch.epoch_no,
        "status": match epoch.status {
            ResearchEpochStatus::Active => "active",
            ResearchEpochStatus::Frozen => "frozen",
        },
        "best_candidate_id": epoch.best_candidate_id.map(|id| id.to_string()),
        "brief": brief.as_ref().map(|brief| serde_json::json!({
            "epoch_no": brief.epoch_no,
            "reset_trigger": format!("{:?}", brief.reset_trigger),
            "open_goals": brief.open_goals,
            "carried_candidates": brief.carried_candidates.len(),
            "excluded_failure_entries": brief.excluded_failure_entries,
        })),
        "reset_from_epoch": epoch.reset_from_epoch.map(|id| id.to_string()),
        "frozen_at_ms": epoch.frozen_at_ms,
    })
}

// ---------------------------------------------------------------------------
// research.candidate
// ---------------------------------------------------------------------------

const CANDIDATE_CONTRACT: &[u8] = b"shenko.host.research-candidate/v1: one idempotent candidate fact recorded for the active epoch; the artifact is content-addressed and reproducible";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateInput {
    candidate_id: EntityId,
    epoch_id: EntityId,
    artifact_ref: ObjectRef,
    score: Option<f64>,
    score_source: Option<String>,
    environment: ResearchEnvironment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenCandidate {
    schema_version: u32,
    candidate_id: EntityId,
    epoch_id: EntityId,
    artifact_ref: ObjectRef,
    score: Option<f64>,
    score_source: Option<String>,
    environment: ResearchEnvironment,
    created_at_ms: i64,
}

pub struct ResearchCandidateHandler {
    store: Arc<Store>,
}

impl ResearchCandidateHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn plugin_id() -> EntityId {
        "8b4c9d51-3e2f-4a6b-8d7c-2f3a4b5c6d7e"
            .parse()
            .expect("fixed built-in research-candidate plugin UUID")
    }

    fn freeze(&self, input: &serde_json::Value) -> Result<FrozenCandidate> {
        let input = CandidateInput::deserialize(input)?;
        if let Some(score) = input.score {
            ensure!(score.is_finite(), "candidate score must be finite");
        }
        if let Some(source) = &input.score_source {
            ensure!(
                !source.trim().is_empty() && source.len() <= 512,
                "score_source must be non-empty and at most 512 bytes"
            );
        }
        Ok(FrozenCandidate {
            schema_version: 1,
            candidate_id: input.candidate_id,
            epoch_id: input.epoch_id,
            artifact_ref: input.artifact_ref,
            score: input.score,
            score_source: input.score_source,
            environment: input.environment,
            created_at_ms: chrono::Utc::now().timestamp_millis(),
        })
    }

    fn candidate_of(&self, frozen: &FrozenCandidate) -> Result<ResearchCandidate> {
        let environment_ref = seal(&self.store, &frozen.environment, "research-candidate-environment")?;
        Ok(ResearchCandidate {
            candidate_id: frozen.candidate_id,
            epoch_id: frozen.epoch_id,
            artifact_ref: frozen.artifact_ref.clone(),
            score: frozen.score,
            score_source: frozen.score_source.clone(),
            environment_ref: Some(environment_ref),
            notes: None,
            frozen: false,
            created_at_ms: frozen.created_at_ms,
        })
    }

    fn write(&self, frozen: &FrozenCandidate) -> Result<(serde_json::Value, OperationOutcome)> {
        let candidate = self.candidate_of(frozen)?;
        self.store.add_research_candidate(&candidate)?;
        Ok((
            serde_json::json!({
                "schema_version": 1,
                "status": "recorded",
                "candidate_id": candidate.candidate_id.to_string(),
                "epoch_id": candidate.epoch_id.to_string(),
                "artifact_digest": candidate.artifact_ref.digest.to_string(),
                "score": candidate.score,
                "score_source": candidate.score_source,
            }),
            OperationOutcome::Succeeded,
        ))
    }
}

#[async_trait]
impl NativeTaskHandler for ResearchCandidateHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "research.candidate".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CANDIDATE_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-research-candidate/1".into(),
            authority_refs: vec!["store.research.candidate.own_subject/v1".into()],
            prompt_section: Some(
                "research.candidate/v1 为当前 epoch 记录候选。输入 candidate_id、epoch_id、artifact_ref、score?、score_source?、environment:{environment_id,facts,toolchain,dataset_versions,reproducible}。artifact_ref 是内容寻址对象；score_source 说明评分来源。环境正文独立密封为 CAS 对象，与候选一起供 reset-to-best 校验。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let frozen = self.freeze(&invocation.input)?;
        run_recorded(
            &self.store,
            &invocation,
            CANDIDATE_CONTRACT,
            Self::plugin_id(),
            "research.candidate.v1",
            "research.candidate.execute",
            "research-candidate-observation-v1",
            "research_candidate_own_subject_epoch",
            &frozen,
            move |_store, frozen| self.write(frozen),
        )
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_write(
            &self.store,
            task,
            operation_id,
            "research.candidate.v1",
            |operation| read_frozen::<FrozenCandidate>(&self.store, operation),
            move |_store, frozen| self.write(frozen),
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// research.bank
// ---------------------------------------------------------------------------

const BANK_CONTRACT: &[u8] = b"shenko.host.research-bank/v1: idempotent durable-bank put/list/supersede/tombstone for the subject's own epochs; a tombstone keeps the content readable";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BankInput {
    op: BankOp,
    entry: Option<BankEntryInput>,
    epoch_id: Option<EntityId>,
    entry_id: Option<EntityId>,
    reason: Option<String>,
    selection: Option<BankSelectionInput>,
    scope: Option<String>,
    superseded_by: Option<EntityId>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BankOp {
    Put,
    List,
    Supersede,
    Tombstone,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BankSelectionInput {
    Carried,
    Failures,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BankEntryInput {
    entry_id: EntityId,
    epoch_id: EntityId,
    kind: BankEntryKind,
    content: String,
    source_refs: Vec<String>,
    scope: String,
    confidence: Option<f64>,
    #[serde(default)]
    base_revision: u64,
    supersedes: Option<EntityId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenBank {
    schema_version: u32,
    op: BankOp,
    entry: Option<BankEntryInput>,
    epoch_id: Option<EntityId>,
    entry_id: Option<EntityId>,
    reason: Option<String>,
    selection: BankSelectionInput,
    scope: Option<String>,
    superseded_by: Option<EntityId>,
    created_at_ms: i64,
}

pub struct ResearchBankHandler {
    store: Arc<Store>,
}

impl ResearchBankHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn plugin_id() -> EntityId {
        "9c5d0e62-4f3a-4b7c-9e8d-3a4b5c6d7e8f"
            .parse()
            .expect("fixed built-in research-bank plugin UUID")
    }

    fn freeze(&self, input: &serde_json::Value) -> Result<FrozenBank> {
        let input = BankInput::deserialize(input)?;
        if let Some(entry) = &input.entry {
            ensure!(
                !entry.content.trim().is_empty(),
                "bank entry content must not be empty"
            );
            ensure!(
                entry.content.len() <= 16 * 1024,
                "bank entry content exceeds 16KiB"
            );
            ensure!(
                entry.source_refs.len() <= 64,
                "bank entry carries more than 64 source refs"
            );
            for source in &entry.source_refs {
                ensure!(
                    !source.trim().is_empty() && source.len() <= 512,
                    "each bank source ref must be non-empty and at most 512 bytes"
                );
            }
            if let Some(confidence) = entry.confidence {
                ensure!(
                    (0.0..=1.0).contains(&confidence),
                    "bank confidence must be in 0.0..=1.0"
                );
            }
        }
        if matches!(input.op, BankOp::Put) {
            ensure!(input.entry.is_some(), "bank put requires an entry");
        }
        if matches!(input.op, BankOp::List) {
            ensure!(input.epoch_id.is_some(), "bank list requires an epoch_id");
        }
        if matches!(input.op, BankOp::Supersede) {
            ensure!(
                input.entry_id.is_some() && input.superseded_by.is_some(),
                "bank supersede requires entry_id and superseded_by"
            );
        }
        if matches!(input.op, BankOp::Tombstone) {
            ensure!(input.entry_id.is_some(), "bank tombstone requires entry_id");
        }
        Ok(FrozenBank {
            schema_version: 1,
            op: input.op,
            entry: input.entry,
            epoch_id: input.epoch_id,
            entry_id: input.entry_id,
            reason: input.reason,
            selection: input.selection.unwrap_or(BankSelectionInput::All),
            scope: input.scope,
            superseded_by: input.superseded_by,
            created_at_ms: chrono::Utc::now().timestamp_millis(),
        })
    }

    fn selection_of(frozen: &FrozenBank) -> BankSelection {
        match frozen.selection {
            BankSelectionInput::Carried => BankSelection::Carried,
            BankSelectionInput::Failures => BankSelection::Failures,
            BankSelectionInput::All => BankSelection::All,
        }
    }

    fn write(&self, frozen: &FrozenBank) -> Result<(serde_json::Value, OperationOutcome)> {
        match frozen.op {
            BankOp::Put => {
                let entry_input = frozen
                    .entry
                    .as_ref()
                    .context("bank put requires an entry")?;
                let entry = InterventionBankEntry {
                    entry_id: entry_input.entry_id,
                    epoch_id: entry_input.epoch_id,
                    kind: entry_input.kind,
                    content: entry_input.content.clone(),
                    source_refs: entry_input.source_refs.clone(),
                    scope: entry_input.scope.clone(),
                    confidence: entry_input.confidence,
                    base_revision: entry_input.base_revision,
                    supersedes: entry_input.supersedes,
                    superseded_by: None,
                    carried_from: None,
                    tombstoned_at_ms: None,
                    created_at_ms: frozen.created_at_ms,
                };
                self.store.put_bank_entry(&entry)?;
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "recorded",
                        "entry_id": entry.entry_id.to_string(),
                        "kind": entry.kind.as_str(),
                        "supersedes": entry.supersedes.map(|id| id.to_string()),
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
            BankOp::List => {
                let epoch_id = frozen.epoch_id.context("bank list requires an epoch_id")?;
                let scope = frozen.scope.as_deref();
                let entries = self
                    .store
                    .bank_entries(epoch_id, Self::selection_of(frozen))?;
                let entries: Vec<_> = entries
                    .iter()
                    .filter(|entry| {
                        scope.is_none_or(|scope| scope == entry.scope)
                    })
                    .collect();
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "listed",
                        "epoch_id": epoch_id.to_string(),
                        "entries": entries
                            .iter()
                            .map(|entry| serde_json::json!({
                                "entry_id": entry.entry_id.to_string(),
                                "kind": entry.kind.as_str(),
                                "scope": entry.scope,
                                "superseded_by": entry.superseded_by.map(|id| id.to_string()),
                                "tombstoned": entry.tombstoned_at_ms.is_some(),
                                "content": entry.content,
                            }))
                            .collect::<Vec<_>>(),
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
            BankOp::Supersede => {
                let entry_id = frozen.entry_id.context("bank supersede requires entry_id")?;
                let superseded_by = frozen
                    .superseded_by
                    .context("bank supersede requires superseded_by")?;
                self.store.supersede_bank_entry(entry_id, superseded_by)?;
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "superseded",
                        "entry_id": entry_id.to_string(),
                        "superseded_by": superseded_by.to_string(),
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
            BankOp::Tombstone => {
                let entry_id = frozen
                    .entry_id
                    .context("bank tombstone requires entry_id")?;
                let reason = frozen.reason.as_deref().unwrap_or("tombstoned");
                self.store.tombstone_bank_entry(entry_id, reason)?;
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "tombstoned",
                        "entry_id": entry_id.to_string(),
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
        }
    }
}

#[async_trait]
impl NativeTaskHandler for ResearchBankHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "research.bank".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(BANK_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-research-bank/1".into(),
            authority_refs: vec!["store.research.bank.own_subject/v1".into()],
            prompt_section: Some(
                "research.bank/v1 维护研究 epoch 的持久事实。put 的 entry 包含 entry_id、epoch_id、kind=requirement|environment_fact|attempt|diagnosis|open_goal、content、source_refs、scope（原样保存的自由字符串）、confidence?、base_revision?、supersedes?。list 指定 epoch_id，可按 selection=all|carried|failures 与精确 scope 过滤；all 返回未立墓碑条目。supersede 指定 entry_id 和 superseded_by；tombstone 指定 entry_id 和 reason。reset 只带入 requirement、environment_fact、open_goal，attempt/diagnosis 保留在旧 epoch 中供审计。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let frozen = self.freeze(&invocation.input)?;
        run_recorded(
            &self.store,
            &invocation,
            BANK_CONTRACT,
            Self::plugin_id(),
            "research.bank.v1",
            "research.bank.execute",
            "research-bank-observation-v1",
            "research_bank_own_subject_epoch",
            &frozen,
            move |_store, frozen| self.write(frozen),
        )
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_write(
            &self.store,
            task,
            operation_id,
            "research.bank.v1",
            |operation| read_frozen::<FrozenBank>(&self.store, operation),
            move |_store, frozen| self.write(frozen),
        )
        .await
    }
}
