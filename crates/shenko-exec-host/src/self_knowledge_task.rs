//! Read-only native self-knowledge queries — SPEC §38, R124.
//! The subject asks what it is, what is installed and why it works this way;
//! this deterministic child answers from the installed SelfKnowledgeProvider,
//! never from model-supplied answers and never with a second opinion layer.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_awareness_core::{SelfKnowledgeProvider, SelfResource, SelfResourceKind};
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
use std::sync::Arc;

const CONTRACT: &[u8] = b"shenko.self.knowledge/v1: bounded reads of installed self:// resources; no external effects";
const MAX_QUERY_BYTES: usize = 4096;
const MAX_LIMIT: usize = 20;
/// Preview of one resource's content in search/list results. The preview is
/// honest about truncation; full bounded content uses `op=get`.
const MAX_PREVIEW_CHARS: usize = 200;
/// Full content returned by `op=get`; longer bodies are truncated and marked.
const MAX_CONTENT_CHARS: usize = 4000;
const MAX_NORMALIZED_BYTES: usize = 480 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum KnowledgeOp {
    /// Substring search over uri/title/content (provider `query`).
    Search,
    /// Index all installed resources, optionally one kind.
    List,
    /// Exact resource content by self:// URI.
    Get,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KnowledgeInput {
    op: KnowledgeOp,
    query: Option<String>,
    uri: Option<String>,
    kind: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FrozenQuery {
    Search { query: String },
    List { kind: Option<SelfResourceKind>, limit: usize },
    Get { uri: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenInput {
    schema_version: u32,
    query: FrozenQuery,
}

/// Deterministic reader over the installed self-knowledge index.
/// Provider contents are instance state, not a claim about the deployed
/// product; the install side in `main` records what was actually installed.
pub struct SelfKnowledgeHandler {
    store: Arc<Store>,
    knowledge: Arc<SelfKnowledgeProvider>,
}

impl SelfKnowledgeHandler {
    pub fn new(store: Arc<Store>, knowledge: Arc<SelfKnowledgeProvider>) -> Self {
        Self { store, knowledge }
    }

    fn plugin_id() -> EntityId {
        "3d2b7c1e-9a4f-4e8b-b5d1-8a2c6f0e4b71"
            .parse()
            .expect("fixed self-knowledge plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            bytes,
            "application/json",
            "native-self-knowledge",
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

    fn freeze(&self, value: serde_json::Value) -> Result<FrozenInput> {
        let KnowledgeInput {
            op,
            query,
            uri,
            kind,
            limit,
        } = serde_json::from_value(value)?;
        let limit_was_specified = limit.is_some();
        let limit = limit.unwrap_or(10);
        ensure!((1..=MAX_LIMIT).contains(&limit), "limit must be 1..=20");
        let kind = kind
            .map(|kind| {
                SelfResourceKind::from_str(&kind)
                    .ok_or_else(|| anyhow::anyhow!("unknown resource kind '{kind}'"))
            })
            .transpose()?;
        let query = match op {
            KnowledgeOp::Search => {
                ensure!(
                    uri.is_none() && kind.is_none(),
                    "search accepts only query and limit"
                );
                let query = query.context("search requires query")?;
                let query = query.trim();
                ensure!(!query.is_empty(), "search query must not be empty");
                ensure!(query.len() <= MAX_QUERY_BYTES, "search query exceeds 4096 bytes");
                FrozenQuery::Search {
                    query: query.into(),
                }
            }
            KnowledgeOp::List => {
                ensure!(
                    query.is_none() && uri.is_none(),
                    "list accepts only kind and limit"
                );
                FrozenQuery::List { kind, limit }
            }
            KnowledgeOp::Get => {
                ensure!(
                    query.is_none() && kind.is_none() && !limit_was_specified,
                    "get accepts exactly one uri"
                );
                let uri = uri.context("get requires uri")?;
                let uri = uri.trim();
                ensure!(uri.starts_with("self://"), "get uri must be a self:// resource");
                FrozenQuery::Get { uri: uri.into() }
            }
        };
        Ok(FrozenInput {
            schema_version: 1,
            query,
        })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "self_knowledge_frozen_input_mismatch"
        );
        let frozen: FrozenInput = serde_json::from_slice(&bytes)?;
        ensure!(
            frozen.schema_version == 1,
            "unsupported_self_knowledge_input_schema"
        );
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "self_knowledge.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "read_only_self_resources".into(),
            extra: serde_json::json!({}),
        };
        let resource = self.seal(&bindings, "self-knowledge-binding")?;
        let acceptance = self.seal(
            &"a bounded read of the installed self:// resources, or an explicit read failure",
            "self-knowledge-acceptance",
        )?;
        let retry = self.seal(
            &"re-read only the frozen query under the original armed attempt; never arm again",
            "self-knowledge-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:self-knowledge-v1".into(),
            },
            symbol: "self.knowledge.query.execute".into(),
            schema: SchemaRef {
                name: "self-knowledge-observation-v1".into(),
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
            operation_name: "self.knowledge.query.v1".into(),
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

    fn query(&self, frozen: &FrozenInput) -> Result<(serde_json::Value, OperationOutcome)> {
        match &frozen.query {
            FrozenQuery::Search { query } => {
                let resources = self.knowledge.query(query);
                Ok((
                    collection_result("search", &resources[..resources.len().min(MAX_LIMIT)]),
                    OperationOutcome::Succeeded,
                ))
            }
            FrozenQuery::List { kind, limit } => {
                let mut resources: Vec<SelfResource> = match kind {
                    Some(kind) => self.knowledge.list(*kind),
                    None => [
                        SelfResourceKind::Runtime,
                        SelfResourceKind::Docs,
                        SelfResourceKind::Source,
                        SelfResourceKind::Design,
                    ]
                    .iter()
                    .flat_map(|kind| self.knowledge.list(*kind))
                    .collect(),
                };
                resources.sort_by(|a, b| a.uri.cmp(&b.uri));
                resources.truncate(*limit);
                Ok((
                    collection_result("list", &resources),
                    OperationOutcome::Succeeded,
                ))
            }
            FrozenQuery::Get { uri } => match self.knowledge.get(uri) {
                Some(resource) => Ok((get_result(&resource), OperationOutcome::Succeeded)),
                None => Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "failed",
                        "op": "get",
                        "requested": {"uri": uri},
                        "error": {"code": "resource_not_found", "message": "no such self:// resource installed"}
                    }),
                    OperationOutcome::Failed,
                )),
            },
        }
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenInput,
    ) -> Result<OperationRecord> {
        let (result, mut outcome) = self.query(frozen)?;
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "self-knowledge-observation")?;
        let normalized = if bytes.len() <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            outcome = OperationOutcome::Failed;
            self.seal(
                &serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "error": {
                        "code": "normalized_result_too_large",
                        "message": "the exact observation is sealed in raw evidence; increase the bounded native result contract before delivery"
                    }
                }),
                "self-knowledge-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "self.knowledge.query.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("self-knowledge:{}", attempt.attempt_id)),
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
            invocation.task.kind == "self.knowledge.query"
                && invocation.task.definition_version == 1,
            "self_knowledge_task_identity_mismatch"
        );
        let frozen = self.freeze(std::mem::take(&mut invocation.input))?;
        let input_ref = self.seal(&frozen, "self-knowledge-frozen-input")?;
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
impl NativeTaskHandler for SelfKnowledgeHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "self.knowledge.query".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-self-knowledge-reader/1".into(),
            authority_refs: vec!["self_knowledge.read/v1".into()],
            prompt_section: Some(
                "self.knowledge.query/v1 是已安装的只读自解释 child。搜索：[delegate:self.knowledge.query:1] {\"op\":\"search\",\"query\":\"能力\"}；列出资料用 op=list（可选 kind: runtime/docs/source/design）；按 URI 取正文用 op=get 且只给一个 self:// URI。结果经后续真实 task_result 返回；search/list 只有有界 content_preview 与截断标志，正文用 get 读取。这些是安装的自身资料，回答以实际安装与来源为准；没命中的能力不凭印象宣称存在。"
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
            operation.task_id == task.task_id
                && operation.operation_name == "self.knowledge.query.v1",
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

fn resource_summary(resource: &SelfResource) -> serde_json::Value {
    let mut chars = resource.content.chars();
    let preview: String = chars.by_ref().take(MAX_PREVIEW_CHARS).collect();
    serde_json::json!({
        "uri": resource.uri,
        "kind": resource.kind.as_str(),
        "title": resource.title,
        "status": resource.status.as_str(),
        "source_revision": resource.source_revision,
        "content_preview": preview,
        "content_preview_truncated": chars.next().is_some(),
    })
}

fn collection_result(op: &str, resources: &[SelfResource]) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "status": "ok",
        "op": op,
        "resources": resources.iter().map(resource_summary).collect::<Vec<_>>(),
    })
}

fn get_result(resource: &SelfResource) -> serde_json::Value {
    let mut chars = resource.content.chars();
    let content: String = chars.by_ref().take(MAX_CONTENT_CHARS).collect();
    serde_json::json!({
        "schema_version": 1,
        "status": "ok",
        "op": "get",
        "resource": {
            "uri": resource.uri,
            "kind": resource.kind.as_str(),
            "title": resource.title,
            "status": resource.status.as_str(),
            "source_revision": resource.source_revision,
            "content": content,
            "content_truncated": chars.next().is_some(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_store::TaskInvocation;
    use shenko_types::agent::CallContext;
    use shenko_types::store::WriteBatch;
    use shenko_types::task::{DispatchEligibility, RunMode, TaskOwner, TaskRevision, TaskState};

    /// Search/list/get answer from installed resources only; a misspelt kind
    /// or unknown self:// URI is an explicit failure, not an empty verdict.
    #[test]
    fn queries_answer_from_installed_resources_and_reject_unknown_pointers() {
        let dir = std::env::temp_dir().join(format!("shenko-self-knowledge-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        let knowledge = Arc::new(SelfKnowledgeProvider::new("test", "0.6.2"));
        let handler = SelfKnowledgeHandler::new(store.clone(), knowledge.clone());

        // The runtime resource is not a template: it reflects the registries
        // the caller actually populated.
        knowledge.install_runtime_capabilities(
            &[("self.knowledge.query".into(), 1)],
            &[],
        );

        let query = |input: serde_json::Value| -> Result<(serde_json::Value, OperationOutcome)> {
            let frozen = handler.freeze(input)?;
            handler.query(&frozen)
        };

        let (listed, _) = query(serde_json::json!({"op":"list"})).unwrap();
        assert_eq!(listed["status"], "ok");
        let uris: Vec<_> = listed["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        assert!(uris.contains(&"self://runtime/capabilities"));
        assert!(uris.contains(&"self://design/philosophy"));

        let (search, _) = query(serde_json::json!({"op":"search","query":"thought loop"})).unwrap();
        assert!(search["resources"].as_array().unwrap().iter().any(|r| r["uri"] == "self://design/philosophy"));

        let (get, outcome) =
            query(serde_json::json!({"op":"get","uri":"self://runtime/capabilities"})).unwrap();
        assert_eq!(outcome, OperationOutcome::Succeeded);
        assert!(
            get["resource"]["content"]
                .as_str()
                .unwrap()
                .contains("self.knowledge.query:1")
        );

        let (miss, outcome) =
            query(serde_json::json!({"op":"get","uri":"self://runtime/teleportation"})).unwrap();
        assert_eq!(outcome, OperationOutcome::Failed);
        assert_eq!(miss["error"]["code"], "resource_not_found");

        // A filter outside the installed kinds is rejected at freeze time.
        assert!(handler
            .freeze(serde_json::json!({"op":"list","kind":"psychic"}))
            .is_err());

        drop(handler);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// T1 arms the original attempt; reconcile re-observes the frozen query on
    /// the same attempt and records exactly one piece of evidence.
    #[tokio::test]
    async fn armed_self_knowledge_query_recovers_on_original_attempt() {
        let dir = std::env::temp_dir().join(format!("shenko-self-knowledge-recovery-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let (db, cas) = (dir.join("shenko.db"), dir.join("cas"));
        let agent_id = EntityId::new();
        let store = Arc::new(Store::open(db.clone(), cas.clone()).unwrap());
        let knowledge = Arc::new(SelfKnowledgeProvider::new("test", "0.6.2"));
        let handler = SelfKnowledgeHandler::new(store.clone(), knowledge.clone());
        let frozen = handler
            .freeze(serde_json::json!({"op":"get","uri":"self://design/philosophy"}))
            .unwrap();
        let input_ref = handler.seal(&frozen, "self-knowledge-frozen-input").unwrap();
        let (task_id, operation_id, invocation_id) =
            (EntityId::new(), EntityId::new(), EntityId::new());
        let epoch = ControllerEpoch(1);
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let task = TaskRecord {
            record_schema_version: 1,
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            agent_id,
            conversation_id: agent_id,
            owner: TaskOwner::Task {
                id: EntityId::new(),
            },
            background: false,
            kind: "self.knowledge.query".into(),
            definition_version: 1,
            implementation_ref: handler.descriptor().implementation_ref,
            input_ref: input_ref.clone(),
            state: TaskState::Running {
                checkpoint_ref: None,
                invocation_id,
                mode: RunMode::Run,
                epoch,
            },
            abort_request: None,
            memos: serde_json::Map::new(),
            residual_case_ids: vec![],
            created_by_step_id: None,
            dispatch_eligibility: DispatchEligibility::Ready,
            no_progress_crashes: 0,
            first_run_at: None,
            terminal_at: None,
            created_commit: zero,
            updated_commit: zero,
        };
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task.clone());
        store.commit(batch).unwrap();
        let task_invocation = TaskInvocation {
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            invocation_id,
            epoch,
            worker_id: EntityId::new(),
            node_id: EntityId::new(),
        };
        let mut context = CallContext::new_system(RuntimeGeneration::default());
        context.task_id = Some(task_id);
        context.invocation_id = Some(invocation_id);
        context.controller_epoch = epoch;
        let invocation = NativeTaskInvocation {
            context,
            task_invocation: task_invocation.clone(),
            task,
            operation_id,
            authority: AuthorityRef {
                delegation_id: task_id,
                delegation_revision: RecordRevision(0),
                decision_ref: EntityId::new(),
            },
            input: serde_json::Value::Null,
        };
        let (mut operation, mut attempt) =
            handler.operation_parts(&invocation, input_ref).unwrap();
        assert!(
            store
                .arm_external_operation(&task_invocation, &mut operation, &mut attempt)
                .unwrap()
                .is_none()
        );
        let attempt_id = attempt.attempt_id;
        assert!(
            store
                .external_operation_evidence(operation_id)
                .unwrap()
                .is_empty()
        );

        // Deterministic crash point: T1 is durable, no observation exists.
        drop(handler);
        drop(knowledge);
        drop(store);
        let reopened = Arc::new(Store::open(db, cas).unwrap());
        let reopened_knowledge = Arc::new(SelfKnowledgeProvider::new("test", "0.6.2"));
        let recovered = SelfKnowledgeHandler::new(reopened.clone(), reopened_knowledge);
        let task = reopened.external_task(task_id).unwrap();
        let resolved = recovered.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(resolved.state, OperationState::Resolved);
        assert_eq!(resolved.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(resolved.effect_status, EffectStatus::None);
        assert_eq!(
            reopened
                .external_operation_attempts(operation_id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reopened
                .external_operation_attempt(operation_id)
                .unwrap()
                .attempt_id,
            attempt_id
        );
        let evidence = reopened.external_operation_evidence(operation_id).unwrap();
        assert_eq!(evidence.len(), 1);
        let normalized = evidence[0].normalized_ref.as_ref().unwrap();
        let result: serde_json::Value =
            serde_json::from_slice(&reopened.cas.read_object(&normalized.digest).unwrap()).unwrap();
        assert_eq!(
            result
                .pointer("/resource/uri")
                .and_then(serde_json::Value::as_str),
            Some("self://design/philosophy")
        );

        // Settlement replay reuses E1 instead of adding another observation.
        let settled = recovered.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(settled.state, OperationState::Resolved);
        assert_eq!(
            reopened
                .external_operation_evidence(operation_id)
                .unwrap()
                .len(),
            1
        );
        drop(recovered);
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }
}
