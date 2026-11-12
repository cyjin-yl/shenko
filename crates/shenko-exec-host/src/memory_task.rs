//! Read-only native memory queries over the caller's own memory (docs/specs/memory.md#spec-12).
//! The subject is derived from the task, never accepted as model input; all
//! three operations observe durable rows and seal the exact observation.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_memory::{MemoryBackend, MemoryHit, MemorySupersessionStatus, RecallQuery, StoreMemoryBackend};
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
use std::sync::Arc;

const CONTRACT: &[u8] = b"shenko.memory.query/v1: immutable own-subject memory reads; no external effects; bounded match-centred excerpts; no audience acl";
const MAX_QUERY_BYTES: usize = 4096;
const MAX_FILTER_VALUES: usize = 8;
const MAX_FILTER_VALUE_BYTES: usize = 64;
const MAX_LIMIT: usize = 50;
const MAX_NORMALIZED_BYTES: usize = 480 * 1024;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MemoryOp {
    Search,
    Read,
    Lineage,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryInput {
    op: MemoryOp,
    query: Option<String>,
    entry_id: Option<EntityId>,
    limit: Option<usize>,
    #[serde(default)]
    knowledge_kinds: Vec<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    include_retired: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FrozenMemoryQuery {
    Search {
        query: String,
        knowledge_kinds: Vec<String>,
        scopes: Vec<String>,
        include_retired: bool,
        limit: usize,
    },
    Read {
        entry_id: EntityId,
    },
    Lineage {
        entry_id: EntityId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenMemoryInput {
    schema_version: u32,
    agent_id: EntityId,
    query: FrozenMemoryQuery,
}

pub struct MemoryHandler {
    store: Arc<Store>,
}

impl MemoryHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn plugin_id() -> EntityId {
        "c44de59e-7b0d-4c3a-9a55-7a0d0dd0b1a2"
            .parse()
            .expect("fixed memory plugin UUID")
    }

    fn backend(&self, agent_id: EntityId) -> StoreMemoryBackend {
        StoreMemoryBackend::new(agent_id, Arc::clone(&self.store))
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            bytes,
            "application/json",
            "native-memory-reads",
            schema,
            1,
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

    fn freeze(&self, value: serde_json::Value, agent_id: EntityId) -> Result<FrozenMemoryInput> {
        let MemoryInput {
            op,
            query,
            entry_id,
            limit,
            mut knowledge_kinds,
            mut scopes,
            include_retired,
        } = serde_json::from_value(value)?;
        let limit_was_specified = limit.is_some();
        let limit = limit.unwrap_or(20);
        ensure!((1..=MAX_LIMIT).contains(&limit), "limit must be 1..=50");
        for values in [&knowledge_kinds, &scopes] {
            ensure!(values.len() <= MAX_FILTER_VALUES, "too many filter values");
            ensure!(
                values.iter().all(|value| {
                    !value.trim().is_empty() && value.len() <= MAX_FILTER_VALUE_BYTES
                }),
                "filter values must be non-empty and at most 64 bytes"
            );
        }
        knowledge_kinds.sort();
        knowledge_kinds.dedup();
        scopes.sort();
        scopes.dedup();
        let query = match op {
            MemoryOp::Search => {
                ensure!(entry_id.is_none(), "search does not accept an entry pointer");
                let query = query.context("search requires query")?;
                let query = query.trim();
                ensure!(!query.is_empty(), "search query must not be empty");
                ensure!(
                    query.len() <= MAX_QUERY_BYTES,
                    "search query exceeds 4096 bytes"
                );
                FrozenMemoryQuery::Search {
                    query: query.into(),
                    knowledge_kinds,
                    scopes,
                    include_retired,
                    limit,
                }
            }
            MemoryOp::Read => {
                ensure!(
                    query.is_none() && !include_retired && !limit_was_specified
                        && knowledge_kinds.is_empty() && scopes.is_empty(),
                    "read accepts exactly one entry_id without search filters"
                );
                let entry_id = entry_id.context("read requires entry_id")?;
                FrozenMemoryQuery::Read { entry_id }
            }
            MemoryOp::Lineage => {
                ensure!(
                    query.is_none() && !include_retired && !limit_was_specified
                        && knowledge_kinds.is_empty() && scopes.is_empty(),
                    "lineage accepts exactly one entry_id without search filters"
                );
                let entry_id = entry_id.context("lineage requires entry_id")?;
                FrozenMemoryQuery::Lineage { entry_id }
            }
        };
        Ok(FrozenMemoryInput { schema_version: 1, agent_id, query })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenMemoryInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "memory_frozen_input_mismatch"
        );
        let frozen: FrozenMemoryInput = serde_json::from_slice(&bytes)?;
        ensure!(
            frozen.schema_version == 1,
            "unsupported_memory_input_schema"
        );
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        agent_id: EntityId,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "store.memory_entries.read.own_subject/v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "read_only_own_subject_memory".into(),
            extra: serde_json::json!({"agent_id": agent_id}),
        };
        let resource = self.seal(&bindings, "memory-query-binding")?;
        let acceptance = self.seal(
            &"a bounded read of the caller's own durable memory rows, or an explicit read failure",
            "memory-query-acceptance",
        )?;
        let retry = self.seal(
            &"re-read only the frozen immutable rows under the original armed attempt; never arm again",
            "memory-query-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:memory-query-v1".into(),
            },
            symbol: "memory.query.execute".into(),
            schema: SchemaRef {
                name: "memory-query-observation-v1".into(),
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
            operation_name: "memory.query.v1".into(),
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

    fn hit_json(hit: &MemoryHit) -> serde_json::Value {
        let status = match hit.supersession {
            MemorySupersessionStatus::Current => "current",
            MemorySupersessionStatus::Superseded => "superseded",
            MemorySupersessionStatus::Superseding => "superseding",
        };
        serde_json::json!({
            "entry_id": hit.entry.entry_id,
            "kind": hit.entry.kind,
            "scope": hit.entry.scope,
            "record_revision": hit.entry.record_revision,
            "deleted": hit.entry.deleted,
            "superseded_by": hit.entry.superseded_by,
            "supersedes": hit.entry.supersedes,
            "source": hit.entry.source,
            "applicability": hit.entry.applicability,
            "confidence": hit.entry.confidence,
            "git_commit": hit.entry.git_commit,
            "git_path": hit.entry.git_path,
            "supersession_status": status,
            "excerpt": hit.excerpt,
            "match_visible": hit.excerpt.as_ref().map(|e| e.match_visible),
        })
    }

    fn query(&self, frozen: &FrozenMemoryInput) -> Result<(serde_json::Value, OperationOutcome)> {
        let backend = self.backend(frozen.agent_id);
        match &frozen.query {
            FrozenMemoryQuery::Search {
                query,
                knowledge_kinds,
                scopes,
                include_retired,
                limit,
            } => {
                let hits = backend.recall(&RecallQuery {
                    text: query.clone(),
                    knowledge_kinds: knowledge_kinds.clone(),
                    scopes: scopes.clone(),
                    include_retired: *include_retired,
                    limit: *limit,
                })?;
                let data: Vec<serde_json::Value> = hits.iter().map(Self::hit_json).collect();
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "op": "search",
                        "agent_id": frozen.agent_id,
                        "query": query,
                        "count": data.len(),
                        "data_preview": data,
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
            FrozenMemoryQuery::Read { entry_id } => {
                let entry = backend.read(*entry_id)?;
                match entry {
                    Some(entry) if entry.agent_id == frozen.agent_id => Ok((
                        serde_json::json!({
                            "schema_version": 1,
                            "op": "read",
                            "entry_id": entry.entry_id,
                            "kind": entry.kind,
                            "scope": entry.scope,
                            "record_revision": entry.record_revision,
                            "deleted": entry.deleted,
                            "superseded_by": entry.superseded_by,
                            "supersedes": entry.supersedes,
                            "source": entry.source,
                            "source_ref": entry.source_ref,
                            "applicability": entry.applicability,
                            "confidence": entry.confidence,
                            "git_commit": entry.git_commit,
                            "git_path": entry.git_path,
                            "content": entry.content,
                        }),
                        OperationOutcome::Succeeded,
                    )),
                    _ => Ok((
                        serde_json::json!({
                            "schema_version": 1,
                            "op": "read",
                            "entry_id": entry_id,
                            "status": "not_found",
                        }),
                        OperationOutcome::Failed,
                    )),
                }
            }
            FrozenMemoryQuery::Lineage { entry_id } => {
                let chain = backend.lineage(*entry_id)?;
                let data: Vec<serde_json::Value> = chain
                    .iter()
                    .map(|entry| {
                        serde_json::json!({
                            "entry_id": entry.entry_id,
                            "record_revision": entry.record_revision,
                            "deleted": entry.deleted,
                            "superseded_by": entry.superseded_by,
                            "supersedes": entry.supersedes,
                        })
                    })
                    .collect();
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "op": "lineage",
                        "entry_id": entry_id,
                        "count": data.len(),
                        "chain": data,
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
        }
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenMemoryInput,
    ) -> Result<OperationRecord> {
        let (result, mut outcome) = self.query(frozen)?;
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "memory-query-observation")?;
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
                        "message": "the exact observation is sealed in raw evidence; increase the bounded native result contract before delivery"
                    }
                }),
                "memory-query-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "memory.query.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("memory-query:{}", attempt.attempt_id)),
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
            invocation.task.kind == "memory.query" && invocation.task.definition_version == 1,
            "memory_task_identity_mismatch"
        );
        let agent_id = invocation.task.agent_id;
        let (head, _) = self.store.load_subject(agent_id)?;
        let step_id = invocation
            .task
            .created_by_step_id
            .context("memory_task_has_no_subject_step")?;
        self.store
            .get_step(&head.branch_id, step_id)?
            .context("memory_task_origin_is_outside_current_subject_branch")?;
        let frozen = self.freeze(std::mem::take(&mut invocation.input), agent_id)?;
        let input_ref = self.seal(&frozen, "memory-query-frozen-input")?;
        let (mut operation, mut attempt) =
            self.operation_parts(&invocation, agent_id, input_ref)?;
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
impl NativeTaskHandler for MemoryHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "memory.query".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-memory-reader/1".into(),
            authority_refs: vec!["store.memory_entries.read.own_subject/v1".into()],
            prompt_section: Some(
                "memory.query/v1 是已安装的只读自有记忆 child。搜索：[delegate:memory.query:1] {\"op\":\"search\",\"query\":\"深刻\"}；读全文用 op=read 且给 entry_id；查修正链用 op=lineage 且给 entry_id。可加 knowledge_kinds/scopes 数组过滤、include_retired=true 查看已遗忘与被取代条目。search 结果带以匹配为中心的预览、截断标志与 match_visible；预览没显示某内容不证明原文没有，需要完整正文时使用 read。不得把尚未返回的回忆当成已核实事实。"
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
            operation.task_id == task.task_id && operation.operation_name == "memory.query.v1",
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
